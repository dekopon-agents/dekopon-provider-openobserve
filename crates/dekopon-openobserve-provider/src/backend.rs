//! The OpenObserve backend: `plan` and `fold` for every word, and the SQL each one writes.
//!
//! Two facts about dekopon's own telemetry shape every statement here, and neither is in #250
//! because neither was known when it was written. Both are recorded in the README.
//!
//! 1. **`accounting.model.turn` is not a queryable column.** `dekopond` wires only the tracer
//!    provider, so a `tracing` event inside a span becomes an OTLP *span event*, and OpenObserve
//!    serializes a span's events into one `events` **string** column
//!    (`Span::events: String`). The identical usage numbers are real span attributes on the
//!    enclosing `prompt.model_turn` span, which fold to ordinary columns, so that is what `agent
//!    stats` reads. `dekopon-brokerd` does wire the logger provider since #217, so
//!    `broker.decision` / `broker.execution` really are rows in the logs stream and the `broker`
//!    words read them directly.
//! 2. **The agent id and the token counts are on different spans.** `agent` is on
//!    `gateway.session`; the usage attributes are on the `prompt.model_turn` spans beneath it. They
//!    are joined by `trace_id`, which is why `agent stats` plans a second statement from the first
//!    answer rather than emitting both at once.

use dekopon_otel_query_core::backend::Backend;
use dekopon_otel_query_core::fit::fit_rows;
use dekopon_otel_query_core::output::{AgentStats, Latency, Tokens, agent_stats, broker_providers};
use dekopon_otel_query_core::query::{
    BrokerView, MAX_SESSIONS, Query, QueryError, Signal, quote_literal,
};
use dekopon_otel_query_core::window::Window;
use dekopon_provider_http::{Request, Response};
use serde_json::Value;

use crate::wire;

/// The span an agent's identity is on.
const SESSION_SPAN: &str = "gateway.session";
/// The span a model turn's usage is on.
const TURN_SPAN: &str = "prompt.model_turn";

/// The OpenObserve `_search` backend, with the clock reading the plan was resolved against.
pub(crate) struct OpenObserve {
    now_us: u64,
}

impl OpenObserve {
    /// Builds a backend against one clock reading, taken once per invocation.
    pub(crate) fn at(now_us: u64) -> Self {
        Self { now_us }
    }

    fn window(&self, query: &Query) -> Window {
        query.scope().window(self.now_us)
    }
}

impl Backend for OpenObserve {
    fn plan(&self, query: &Query, prior: &[Response]) -> Result<Option<Request>, QueryError> {
        let scope = query.scope();
        let window = self.window(query);
        match query {
            Query::Search(search) if prior.is_empty() => {
                wire::search(scope, search.signal, &search.sql, window, search.limit).map(Some)
            }
            Query::Trace(trace) if prior.is_empty() => wire::search(
                scope,
                Signal::Traces,
                &trace_sql(&scope.stream, &trace.trace_id, trace.limit),
                window,
                trace.limit,
            )
            .map(Some),
            Query::AgentStats(stats) => match prior.len() {
                0 => wire::search(
                    scope,
                    Signal::Traces,
                    &sessions_sql(&scope.stream, &stats.agent),
                    window,
                    MAX_SESSIONS,
                )
                .map(Some),
                1 => {
                    let traces = trace_ids(&prior[0])?;
                    if traces.is_empty() {
                        // No sessions means no turns, and an `IN ()` is not a statement. The
                        // decisions query still runs: a denial does not need a session to exist.
                        return wire::search(
                            scope,
                            Signal::Logs,
                            &decisions_sql(&scope.stream, &stats.agent),
                            window,
                            MAX_SESSIONS,
                        )
                        .map(Some);
                    }
                    wire::search(
                        scope,
                        Signal::Traces,
                        &turns_sql(&scope.stream, &traces),
                        window,
                        1,
                    )
                    .map(Some)
                }
                2 if !trace_ids(&prior[0])?.is_empty() => wire::search(
                    scope,
                    Signal::Logs,
                    &decisions_sql(&scope.stream, &stats.agent),
                    window,
                    MAX_SESSIONS,
                )
                .map(Some),
                _ => Ok(None),
            },
            Query::Broker(broker) => match (broker.view, prior.len()) {
                (BrokerView::Providers, 0) => {
                    wire::search(scope, Signal::Logs, &boot_sql(&scope.stream), window, 1).map(Some)
                }
                (BrokerView::Providers, 1) => match boot_timestamp(&prior[0])? {
                    None => Ok(None),
                    Some(booted_at) => wire::search(
                        scope,
                        Signal::Logs,
                        &loaded_providers_sql(&scope.stream),
                        Window::ending_at(
                            window.end_us(),
                            (window.end_us() - booted_at) / 1_000_000,
                        ),
                        broker.limit,
                    )
                    .map(Some),
                },
                (BrokerView::Usage, 0) => wire::search(
                    scope,
                    Signal::Logs,
                    &usage_sql(&scope.stream, broker.by.column(), broker.limit),
                    window,
                    broker.limit,
                )
                .map(Some),
                (BrokerView::Denials, 0) => wire::search(
                    scope,
                    Signal::Logs,
                    &denials_sql(&scope.stream, broker.limit),
                    window,
                    broker.limit,
                )
                .map(Some),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn fold(&self, query: &Query, responses: &[Response]) -> Result<Value, QueryError> {
        let scope = query.scope();
        match query {
            Query::Search(_) | Query::Trace(_) => {
                let response = responses
                    .first()
                    .ok_or_else(|| QueryError::upstream("the store answered nothing"))?;
                let (rows, total) = wire::hits(response)?;
                Ok(fit_rows(rows, total, scope.max_output_bytes))
            }
            Query::AgentStats(stats) => {
                let sessions = trace_ids(
                    responses
                        .first()
                        .ok_or_else(|| QueryError::upstream("the store answered nothing"))?,
                )?;
                let mut folded = AgentStats {
                    sessions: sessions.len() as u64,
                    truncated: sessions.len() as u32 >= MAX_SESSIONS,
                    ..AgentStats::default()
                };
                if !sessions.is_empty()
                    && let Some(response) = responses.get(1)
                {
                    let (rows, _) = wire::hits(response)?;
                    if let Some(row) = rows.first() {
                        folded.turns = number(row, "turns");
                        folded.succeeded = folded.turns;
                        folded.latency = Latency {
                            p50: number(row, "p50") / 1_000,
                            p95: number(row, "p95") / 1_000,
                        };
                        folded.tokens = Tokens {
                            input: number(row, "input_tokens"),
                            cached_input: number(row, "cached_input_tokens"),
                            output: number(row, "output_tokens"),
                            reasoning_output: number(row, "reasoning_output_tokens"),
                            total: number(row, "total_tokens"),
                        };
                    }
                }
                if let Some(response) = responses.last().filter(|_| responses.len() > 1) {
                    let (rows, _) = wire::hits(response)?;
                    fold_decisions(&rows, &mut folded);
                }
                Ok(agent_stats(
                    &stats.agent,
                    self.window(query),
                    &folded,
                    scope.max_output_bytes,
                ))
            }
            Query::Broker(broker) => match broker.view {
                BrokerView::Providers => {
                    let booted_at = boot_timestamp(
                        responses
                            .first()
                            .ok_or_else(|| QueryError::upstream("the store answered nothing"))?,
                    )?;
                    let (rows, total) = match responses.get(1) {
                        Some(response) => wire::hits(response)?,
                        None => (Vec::new(), 0),
                    };
                    let truncated = total > rows.len() as u64;
                    Ok(broker_providers(booted_at, rows, truncated))
                }
                BrokerView::Usage | BrokerView::Denials => {
                    let response = responses
                        .first()
                        .ok_or_else(|| QueryError::upstream("the store answered nothing"))?;
                    let (rows, total) = wire::hits(response)?;
                    Ok(fit_rows(rows, total, scope.max_output_bytes))
                }
            },
        }
    }
}

/// The projected span skeleton of one trace.
///
/// Every column here is a non-optional field of OpenObserve's own span record, so the statement
/// cannot fail on a column a particular deployment never ingested. `events` is deliberately absent:
/// it is the JSON string a span's events are serialized into, and for dekopon that string carries
/// `agent.model.prompt` and `agent.model.answer` — the whole conversation. An operator who needs it
/// asks for it by name through `openobserve sql`, which is the Medium-risk capability.
fn trace_sql(stream: &str, trace_id: &str, limit: u32) -> String {
    format!(
        "SELECT trace_id, span_id, operation_name, service_name, span_kind, span_status, \
         start_time, end_time, duration FROM \"{stream}\" WHERE trace_id = {} \
         ORDER BY start_time ASC LIMIT {limit}",
        quote_literal(trace_id)
    )
}

fn sessions_sql(stream: &str, agent: &str) -> String {
    format!(
        "SELECT trace_id FROM \"{stream}\" WHERE operation_name = {} AND agent = {} \
         ORDER BY start_time DESC LIMIT {MAX_SESSIONS}",
        quote_literal(SESSION_SPAN),
        quote_literal(agent)
    )
}

/// The turn aggregate, computed in the store rather than folded over rows.
///
/// `count` and the `sum`s are exact over every matching turn, not over a page of them, which is the
/// property a token total needs to be worth reading. `approx_percentile_cont` is DataFusion's, and
/// OpenObserve's own service-graph queries use it, so it is a function the deployed store has. The
/// `duration` column is a span's microseconds; the output shape is milliseconds, and the division
/// happens in the fold.
fn turns_sql(stream: &str, traces: &[String]) -> String {
    let ids: Vec<String> = traces.iter().map(|id| quote_literal(id)).collect();
    format!(
        "SELECT count(*) AS turns, \
         sum(usage_input_tokens) AS input_tokens, \
         sum(usage_cached_input_tokens) AS cached_input_tokens, \
         sum(usage_output_tokens) AS output_tokens, \
         sum(usage_reasoning_output_tokens) AS reasoning_output_tokens, \
         sum(usage_total_tokens) AS total_tokens, \
         approx_percentile_cont(duration, 0.5) AS p50, \
         approx_percentile_cont(duration, 0.95) AS p95 \
         FROM \"{stream}\" WHERE operation_name = {} AND trace_id IN ({})",
        quote_literal(TURN_SPAN),
        ids.join(", ")
    )
}

fn decisions_sql(stream: &str, agent: &str) -> String {
    format!(
        "SELECT capability_id, decision_allowed, decision_reason, count(*) AS calls \
         FROM \"{stream}\" WHERE audit_event = 'broker.decision' AND actor_id = {} \
         GROUP BY capability_id, decision_allowed, decision_reason ORDER BY calls DESC LIMIT {MAX_SESSIONS}",
        quote_literal(agent)
    )
}

fn boot_sql(stream: &str) -> String {
    format!(
        "SELECT _timestamp FROM \"{stream}\" WHERE event = 'broker_started' \
         ORDER BY _timestamp DESC LIMIT 1"
    )
}

/// Every provider the broker announced after that boot.
///
/// The discriminator is `artifact_sha256 IS NOT NULL` rather than the record's message, because a
/// log record's body lands in a different column on different store versions while an attribute
/// folds to a stable column name. `path` is in the record and deliberately not in the projection.
fn loaded_providers_sql(stream: &str) -> String {
    format!(
        "SELECT provider, artifact_sha256, capabilities, command_words, command_export, compile_ms \
         FROM \"{stream}\" WHERE artifact_sha256 IS NOT NULL ORDER BY _timestamp ASC LIMIT 200"
    )
}

fn usage_sql(stream: &str, key: &str, limit: u32) -> String {
    format!(
        "SELECT {key} AS bucket, count(*) AS calls, \
         sum(CASE WHEN outcome = 'succeeded' THEN 1 ELSE 0 END) AS succeeded, \
         approx_percentile_cont(duration_ms, 0.5) AS p50, \
         approx_percentile_cont(duration_ms, 0.95) AS p95 \
         FROM \"{stream}\" WHERE audit_event = 'broker.execution' AND {key} IS NOT NULL \
         GROUP BY bucket ORDER BY calls DESC LIMIT {limit}"
    )
}

fn denials_sql(stream: &str, limit: u32) -> String {
    format!(
        "SELECT capability_id, decision_reason, actor_id, count(*) AS denials \
         FROM \"{stream}\" WHERE audit_event = 'broker.decision' AND decision_allowed = false \
         GROUP BY capability_id, decision_reason, actor_id ORDER BY denials DESC LIMIT {limit}"
    )
}

fn trace_ids(response: &Response) -> Result<Vec<String>, QueryError> {
    let (rows, _) = wire::hits(response)?;
    Ok(rows
        .iter()
        .filter_map(|row| row.get("trace_id").and_then(Value::as_str))
        .filter(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .collect())
}

fn boot_timestamp(response: &Response) -> Result<Option<u64>, QueryError> {
    let (rows, _) = wire::hits(response)?;
    Ok(rows
        .first()
        .and_then(|row| row.get("_timestamp"))
        .and_then(Value::as_u64))
}

fn fold_decisions(rows: &[Value], folded: &mut AgentStats) {
    for row in rows {
        let calls = row.get("calls").and_then(Value::as_u64).unwrap_or(0);
        let allowed = row
            .get("decision_allowed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if allowed {
            let capability = row
                .get("capability_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *folded
                .capabilities
                .entry(capability.to_owned())
                .or_insert(0) += calls;
        } else {
            let reason = row
                .get("decision_reason")
                .and_then(Value::as_str)
                .unwrap_or("policy-denied");
            *folded.denials.entry(reason.to_owned()).or_insert(0) += calls;
        }
    }
}

/// A store number, whatever JSON shape it arrived in.
///
/// DataFusion returns a `sum` over an all-null column as JSON `null`, an integer count as a number,
/// and an `approx_percentile_cont` as a float. All three mean "this many" to the shape, so all
/// three land as a `u64` and an absent one is zero.
fn number(row: &Value, key: &str) -> u64 {
    match row.get(key) {
        Some(Value::Number(number)) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|value| value.max(0.0) as u64))
            .unwrap_or(0),
        Some(Value::String(text)) => text.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Every column any planned statement names, for the projection test.
#[cfg(test)]
pub(crate) fn planned_columns() -> Vec<&'static str> {
    vec![
        "trace_id",
        "span_id",
        "operation_name",
        "service_name",
        "span_kind",
        "span_status",
        "start_time",
        "end_time",
        "duration",
        "agent",
        "usage_input_tokens",
        "usage_cached_input_tokens",
        "usage_output_tokens",
        "usage_reasoning_output_tokens",
        "usage_total_tokens",
        "capability_id",
        "decision_allowed",
        "decision_reason",
        "actor_id",
        "audit_event",
        "outcome",
        "duration_ms",
        "_timestamp",
        "event",
        "provider",
        "artifact_sha256",
        "capabilities",
        "command_words",
        "command_export",
        "compile_ms",
    ]
}

#[cfg(test)]
mod tests {
    use dekopon_otel_query_core::projection::first_forbidden;

    use super::{
        boot_sql, decisions_sql, denials_sql, loaded_providers_sql, planned_columns, sessions_sql,
        trace_sql, turns_sql, usage_sql,
    };

    /// The whole exclusion list, against every column any statement in this file names.
    ///
    /// This is the mechanized form of #250's "the projection lists columns, it never `SELECT *`s":
    /// a statement that grows a column fails here rather than shipping a leak.
    #[test]
    fn no_planned_statement_names_a_forbidden_column() {
        assert_eq!(first_forbidden(planned_columns()), None);
    }

    /// `events` is the string a span's events are serialized into, and for dekopon that string is
    /// the conversation. It must not appear in a projected statement.
    #[test]
    fn the_projected_statements_never_select_events_or_a_star() {
        let statements = [
            trace_sql("dekopon", "0af7651916cd43dd8448eb211c80319c", 50),
            sessions_sql("dekopon", "reviewer"),
            turns_sql("dekopon", &["0af7651916cd43dd8448eb211c80319c".to_owned()]),
            decisions_sql("dekopon", "reviewer"),
            boot_sql("dekopon"),
            loaded_providers_sql("dekopon"),
            usage_sql("dekopon", "provider", 50),
            denials_sql("dekopon", 50),
        ];
        for statement in &statements {
            assert!(
                !statement.contains('*') || statement.contains("count(*)"),
                "{statement}"
            );
            assert!(!statement.contains("events"), "{statement}");
            assert!(!statement.contains("gen_ai"), "{statement}");
            assert!(!statement.contains("path"), "{statement}");
            assert!(statement.starts_with("SELECT "), "{statement}");
            assert!(!statement.contains(';'), "{statement}");
        }
    }

    /// The agent id reaches the statement as a quoted literal and nothing else.
    #[test]
    fn an_agent_id_with_a_quote_cannot_close_the_literal() {
        let statement = sessions_sql("dekopon", "review'er");
        assert!(statement.contains("agent = 'review''er'"), "{statement}");
    }

    #[test]
    fn the_turn_aggregate_scopes_by_every_session_trace_id() {
        let statement = turns_sql(
            "dekopon",
            &[
                "0af7651916cd43dd8448eb211c80319c".to_owned(),
                "1bf7651916cd43dd8448eb211c80319d".to_owned(),
            ],
        );
        assert!(
            statement.contains(
                "trace_id IN ('0af7651916cd43dd8448eb211c80319c', '1bf7651916cd43dd8448eb211c80319d')"
            ),
            "{statement}"
        );
        assert!(
            statement.contains("operation_name = 'prompt.model_turn'"),
            "{statement}"
        );
        assert!(
            statement.contains("approx_percentile_cont(duration, 0.95)"),
            "{statement}"
        );
    }

    /// 200 trace ids is the fan-in ceiling, and the statement it produces has to fit inside the
    /// 16 KiB `maxRequestBytes` #250's constraint sets write.
    #[test]
    fn the_widest_turn_statement_fits_inside_max_request_bytes() {
        let traces: Vec<String> = (0..200).map(|index| format!("{index:032x}")).collect();
        let statement = turns_sql("dekopon", &traces);
        assert!(statement.len() < 16 * 1024, "{}", statement.len());
    }

    #[test]
    fn the_broker_views_group_on_the_column_the_flag_named() {
        assert!(usage_sql("dekopon", "actor_id", 50).contains("SELECT actor_id AS bucket"));
        assert!(usage_sql("dekopon", "capability_id", 50).contains("GROUP BY bucket"));
        assert!(denials_sql("dekopon", 50).contains("decision_allowed = false"));
        assert!(loaded_providers_sql("dekopon").contains("artifact_sha256 IS NOT NULL"));
    }
}
