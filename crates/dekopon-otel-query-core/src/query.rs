//! The backend-neutral query a word becomes, and the JSON it travels as.
//!
//! A command word produces a capability proposal, and that proposal is authorized on exactly the
//! path a direct `cap openobserve.agent-stats {…}` call takes. So the word layer and the `invoke`
//! layer must agree on one wire shape, and this is it: camelCase keys, closed schemas, no field
//! whose absence changes what is read.
//!
//! camelCase is not a style choice. The sandboxed shell rewrites `--kebab-flags` into camelCase
//! JSON keys before it calls a capability, so a provider that declares snake_case fields with
//! `deny_unknown_fields` can never be called through the flag form — which is precisely the bug
//! `dekopon-provider-mediawiki` v0.1.0 shipped. Declaring the wire in camelCase makes the flag form
//! and the object form land on the same keys.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fit::{DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES_CEILING, MIN_OUTPUT_BYTES};
use crate::window::{Window, parse_since};

/// The largest number of rows any word will ask a store for.
pub const MAX_LIMIT: u32 = 500;
/// The number of rows a word asks for when the caller named none.
pub const DEFAULT_LIMIT: u32 = 100;
/// The default org path segment on an OpenObserve deployment.
pub const DEFAULT_ORG: &str = "default";
/// The stream both dekopon daemons route their traces and logs into.
pub const DEFAULT_STREAM: &str = "dekopon";
/// How many sessions one `agent stats` folds over.
///
/// The second query scopes by `trace_id IN (…)`, so this is also the request-body budget: 200 ids
/// is about 7 KiB of SQL, inside the 16 KiB `maxRequestBytes` #250's constraint sets write. More
/// sessions than this in the window sets `truncated`.
pub const MAX_SESSIONS: u32 = 200;

/// A telemetry signal, which selects the store's `type=` and the shape of a row.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    /// Spans. One row is one span, with folded span attributes as columns.
    Traces,
    /// Log records. One row is one record, with folded attributes as columns.
    Logs,
}

impl Signal {
    /// The value of the store's `type=` query parameter.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Logs => "logs",
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How `broker usage` groups its rows.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageGrouping {
    /// One row per provider id.
    #[default]
    Provider,
    /// One row per capability id.
    Capability,
    /// One row per acting agent.
    Agent,
}

impl UsageGrouping {
    /// The folded column the grouping keys on.
    #[must_use]
    pub fn column(&self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Capability => "capability_id",
            Self::Agent => "actor_id",
        }
    }
}

/// Everything every word carries: where the store is, which stream, and over what window.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Scope {
    /// The store's base URL, without a trailing slash: `https://rpi.lan/openobserve`.
    ///
    /// An argument rather than a build constant, because a self-hosted store has no well-known
    /// address the way `api.github.com` does. Any host outside the owner's `allowedHosts` is a
    /// `Denied` egress recorded on the trace, never a leak.
    pub url: String,
    /// The store's organization path segment.
    #[serde(default = "default_org")]
    pub org: String,
    /// The stream both dekopon daemons export into.
    #[serde(default = "default_stream")]
    pub stream: String,
    /// The window, in seconds, already capped at 30 days.
    pub since_seconds: u64,
    /// The ceiling the result is fitted under.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
}

fn default_org() -> String {
    DEFAULT_ORG.to_owned()
}

fn default_stream() -> String {
    DEFAULT_STREAM.to_owned()
}

fn default_max_output_bytes() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

impl Scope {
    /// Checks the parts a guest can check, and normalizes the URL.
    ///
    /// Deliberately shallow: the broker owns authorization, and a guest that re-implemented host
    /// allowlisting would be asserting a bound it cannot enforce. What is checked here is what
    /// would otherwise produce a confusing upstream error — a scheme the host will refuse anyway,
    /// a URL already carrying the query string this provider appends, an org or stream name that
    /// would need quoting in a path or a SQL identifier.
    pub fn validate(&mut self) -> Result<(), QueryError> {
        self.url = self.url.trim_end_matches('/').to_owned();
        if !(self.url.starts_with("http://") || self.url.starts_with("https://")) {
            return Err(QueryError::invalid(format!(
                "--url {}: expected an http:// or https:// base URL",
                self.url
            )));
        }
        if self.url.contains(['?', '#', ' ']) {
            return Err(QueryError::invalid(
                "--url must be a base URL with no query string or fragment",
            ));
        }
        for (label, value) in [("--org", &self.org), ("--stream", &self.stream)] {
            if value.is_empty()
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err(QueryError::invalid(format!(
                    "{label} {value}: expected letters, digits, underscore, or hyphen"
                )));
            }
        }
        if !(MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES_CEILING).contains(&self.max_output_bytes) {
            return Err(QueryError::invalid(format!(
                "--max-output-bytes {} is outside {MIN_OUTPUT_BYTES}..={MAX_OUTPUT_BYTES_CEILING}",
                self.max_output_bytes
            )));
        }
        Ok(())
    }

    /// Resolves the window against a clock reading in microseconds.
    #[must_use]
    pub fn window(&self, now_us: u64) -> Window {
        Window::ending_at(now_us, self.since_seconds)
    }

    /// Builds a scope from the parts a command word parsed, applying the 30-day cap.
    pub fn from_flags(
        url: String,
        org: String,
        stream: String,
        since: &str,
        max_output_bytes: usize,
    ) -> Result<Self, QueryError> {
        let since_seconds = parse_since(since).map_err(QueryError::invalid)?;
        let mut scope = Self {
            url,
            org,
            stream,
            since_seconds,
            max_output_bytes,
        };
        scope.validate()?;
        Ok(scope)
    }
}

/// One statement against the store, as `openobserve sql` and `openobserve search` both produce it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SearchQuery {
    /// Where and when.
    #[serde(flatten)]
    pub scope: Scope,
    /// Which signal, which selects the store's `type=`.
    pub signal: Signal,
    /// The statement. One statement, no trailing `;`, must start with `SELECT`.
    pub sql: String,
    /// The row ceiling, at most 500.
    #[serde(default = "default_limit")]
    pub limit: u32,
}

/// Every span of one trace.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TraceQuery {
    /// Where and when.
    #[serde(flatten)]
    pub scope: Scope,
    /// The 32-hex-character W3C trace id.
    pub trace_id: String,
    /// The span ceiling, at most 500.
    #[serde(default = "default_limit")]
    pub limit: u32,
}

/// One agent's own numbers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentStatsQuery {
    /// Where and when.
    #[serde(flatten)]
    pub scope: Scope,
    /// The agent whose sessions are folded.
    ///
    /// A flag in 0.1.0, and #248 says why that is second best: Cedar decides on `context.agent`,
    /// which the broker stamps, and it cannot compare provider input against context. So a grant of
    /// `openobserve.agent-stats` lets its holder name any agent. The `dekopon:meta/caller@0.1.0`
    /// import that would make the scope a fact of the invocation does not exist yet.
    pub agent: String,
}

/// The fleet views, all three of them.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrokerQuery {
    /// Where and when.
    #[serde(flatten)]
    pub scope: Scope,
    /// Which view.
    pub view: BrokerView,
    /// How `usage` groups. Ignored by the other two views.
    #[serde(default)]
    pub by: UsageGrouping,
    /// The row ceiling, at most 500.
    #[serde(default = "default_limit")]
    pub limit: u32,
}

/// Which fleet view a `broker` word asked for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BrokerView {
    /// What is loaded, as of the last boot in the window.
    Providers,
    /// Calls, grouped.
    Usage,
    /// Refusals, grouped by reason.
    Denials,
}

fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

/// The typed form of one authorized invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// `openobserve sql` and `openobserve search`.
    Search(SearchQuery),
    /// `openobserve trace`.
    Trace(TraceQuery),
    /// `agent stats`.
    AgentStats(AgentStatsQuery),
    /// `broker providers`, `broker usage`, `broker denials`.
    Broker(BrokerQuery),
}

impl Query {
    /// Where and when, for whichever word this is.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        match self {
            Self::Search(query) => &query.scope,
            Self::Trace(query) => &query.scope,
            Self::AgentStats(query) => &query.scope,
            Self::Broker(query) => &query.scope,
        }
    }

    /// Validates and normalizes every bound the guest owns.
    pub fn validate(&mut self) -> Result<(), QueryError> {
        match self {
            Self::Search(query) => {
                query.scope.validate()?;
                check_limit(query.limit)?;
                query.sql = check_statement(&query.sql)?;
            }
            Self::Trace(query) => {
                query.scope.validate()?;
                check_limit(query.limit)?;
                check_trace_id(&query.trace_id)?;
            }
            Self::AgentStats(query) => {
                query.scope.validate()?;
                check_agent(&query.agent)?;
            }
            Self::Broker(query) => {
                query.scope.validate()?;
                check_limit(query.limit)?;
            }
        }
        Ok(())
    }
}

/// Why a query was refused before it cost a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryError {
    code: &'static str,
    message: String,
}

/// The code for an argument this provider refused.
pub const INVALID_INPUT: &str = "invalid-input";
/// The code for a store that could not be reached or did not answer usefully.
pub const UPSTREAM_FAILURE: &str = "upstream-failure";

impl QueryError {
    /// An argument this provider refused.
    #[must_use]
    pub fn invalid(message: impl fmt::Display) -> Self {
        Self {
            code: INVALID_INPUT,
            message: message.to_string(),
        }
    }

    /// A store that could not be reached or did not answer usefully.
    #[must_use]
    pub fn upstream(message: impl fmt::Display) -> Self {
        Self {
            code: UPSTREAM_FAILURE,
            message: message.to_string(),
        }
    }

    /// The stable classification.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The bounded explanation.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

fn check_limit(limit: u32) -> Result<(), QueryError> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(QueryError::invalid(format!(
            "--limit {limit} is outside 1..={MAX_LIMIT}"
        )));
    }
    Ok(())
}

fn check_trace_id(trace_id: &str) -> Result<(), QueryError> {
    if trace_id.len() != 32 || !trace_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(QueryError::invalid(format!(
            "trace id {trace_id}: expected 32 hexadecimal characters"
        )));
    }
    Ok(())
}

fn check_agent(agent: &str) -> Result<(), QueryError> {
    if agent.is_empty()
        || agent.len() > 128
        || !agent
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(QueryError::invalid(format!(
            "--agent {agent}: expected an agent id of letters, digits, hyphen, or underscore"
        )));
    }
    Ok(())
}

/// The whole SQL policy: one statement, starting with `SELECT`, with no embedded `;`.
///
/// Not a parser, on purpose — #250's non-goals rule one out. It is a gate against the two things
/// that turn one authorized read into something else: a second statement, and a statement that is
/// not a read. Everything past that is the store's problem, and a store that refuses a statement
/// answers with a 400 the caller can read.
pub fn check_statement(sql: &str) -> Result<String, QueryError> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(QueryError::invalid("the statement is empty"));
    }
    if trimmed.len() > 8 * 1024 {
        return Err(QueryError::invalid(format!(
            "the statement is {} bytes; the maximum is 8192",
            trimmed.len()
        )));
    }
    if trimmed.contains(';') {
        return Err(QueryError::invalid(
            "one statement only: `;` is refused anywhere but at the end",
        ));
    }
    let head = trimmed
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if head != "SELECT" && head != "WITH" {
        return Err(QueryError::invalid(format!(
            "the statement starts with {head}; only SELECT (or a WITH that ends in one) is read-only"
        )));
    }
    Ok(trimmed.to_owned())
}

/// Escapes a value for a single-quoted SQL literal.
///
/// Doubling the quote is the whole escape SQL defines for a single-quoted string, and every value
/// this provider interpolates has already been checked into a narrow alphabet — agent ids, trace
/// ids, capability ids out of the store. The doubling is the belt for the braces.
#[must_use]
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::{
        BrokerQuery, BrokerView, Query, QueryError, Scope, SearchQuery, Signal, TraceQuery,
        UsageGrouping, check_statement, quote_literal,
    };
    use crate::fit::DEFAULT_MAX_OUTPUT_BYTES;

    fn scope() -> Scope {
        Scope {
            url: "http://rpi.lan/openobserve".to_owned(),
            org: "default".to_owned(),
            stream: "dekopon".to_owned(),
            since_seconds: 86_400,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// The wire is camelCase in both directions, which is what makes the shell's `--flag` rewriting
    /// land on the right keys. A snake_case field here is the mediawiki v0.1.0 bug.
    #[test]
    fn the_wire_shape_is_camel_case_and_closed() {
        let query = SearchQuery {
            scope: scope(),
            signal: Signal::Traces,
            sql: "SELECT 1".to_owned(),
            limit: 10,
        };
        let json = serde_json::to_value(&query).expect("serializes");
        for key in [
            "url",
            "org",
            "stream",
            "sinceSeconds",
            "maxOutputBytes",
            "signal",
            "sql",
            "limit",
        ] {
            assert!(json.get(key).is_some(), "{key} is missing from {json}");
        }
        for key in json.as_object().expect("an object").keys() {
            assert!(!key.contains('_'), "{key} is snake_case");
        }
        let round_tripped: SearchQuery = serde_json::from_value(json).expect("round trips");
        assert_eq!(round_tripped, query);

        let mut extra = serde_json::to_value(&query).expect("serializes");
        extra["bogus"] = serde_json::json!(1);
        assert!(
            serde_json::from_value::<SearchQuery>(extra).is_err(),
            "unknown fields are refused"
        );
    }

    /// Defaults exist so a model can type the short form, and they are the documented ones.
    #[test]
    fn the_optional_fields_default_to_the_documented_values() {
        let query: SearchQuery = serde_json::from_value(serde_json::json!({
            "url": "http://rpi.lan/openobserve",
            "sinceSeconds": 3_600,
            "signal": "logs",
            "sql": "SELECT 1"
        }))
        .expect("the short form parses");
        assert_eq!(query.scope.org, "default");
        assert_eq!(query.scope.stream, "dekopon");
        assert_eq!(query.scope.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(query.limit, 100);
    }

    #[test]
    fn one_read_only_statement_is_the_whole_sql_policy() {
        assert_eq!(
            check_statement("  SELECT * FROM \"dekopon\" ; ").expect("accepted"),
            "SELECT * FROM \"dekopon\""
        );
        assert_eq!(
            check_statement("with t as (select 1) select * from t").expect("accepted"),
            "with t as (select 1) select * from t"
        );
        for refused in [
            "",
            "   ",
            "DELETE FROM \"dekopon\"",
            "DROP TABLE x",
            "SELECT 1; SELECT 2",
            "INSERT INTO x VALUES (1)",
            "UPDATE x SET y = 1",
        ] {
            assert!(check_statement(refused).is_err(), "{refused} was accepted");
        }
        assert!(check_statement(&"SELECT ".repeat(2_000)).is_err());
    }

    #[test]
    fn the_url_the_org_and_the_stream_are_normalized_or_refused() {
        let mut good = scope();
        good.url = "https://rpi.lan/openobserve/".to_owned();
        good.validate().expect("a trailing slash is trimmed");
        assert_eq!(good.url, "https://rpi.lan/openobserve");

        for url in [
            "rpi.lan/openobserve",
            "ftp://rpi.lan",
            "http://rpi.lan/openobserve?type=traces",
            "http://rpi.lan/open observe",
        ] {
            let mut scope = scope();
            scope.url = url.to_owned();
            assert!(scope.validate().is_err(), "{url} was accepted");
        }
        for stream in ["", "dekopon\"; DROP", "deko pon", "deko.pon"] {
            let mut scope = scope();
            scope.stream = stream.to_owned();
            assert!(scope.validate().is_err(), "{stream} was accepted");
        }
    }

    #[test]
    fn limits_trace_ids_and_agent_ids_are_bounded() {
        let mut query = Query::Trace(TraceQuery {
            scope: scope(),
            trace_id: "0af7651916cd43dd8448eb211c80319c".to_owned(),
            limit: 500,
        });
        query.validate().expect("a well-formed trace query");

        for (trace_id, limit) in [
            ("0af7651916cd43dd8448eb211c80319", 500_u32),
            ("0af7651916cd43dd8448eb211c80319cz", 500),
            ("0af7651916cd43dd8448eb211c80319c", 501),
            ("0af7651916cd43dd8448eb211c80319c", 0),
        ] {
            let mut query = Query::Trace(TraceQuery {
                scope: scope(),
                trace_id: trace_id.to_owned(),
                limit,
            });
            assert!(query.validate().is_err(), "{trace_id}/{limit} was accepted");
        }
    }

    #[test]
    fn a_single_quote_in_a_literal_is_doubled() {
        assert_eq!(quote_literal("review'er"), "'review''er'");
        assert_eq!(quote_literal("reviewer"), "'reviewer'");
    }

    #[test]
    fn the_broker_view_and_grouping_round_trip_through_the_wire() {
        let query = BrokerQuery {
            scope: scope(),
            view: BrokerView::Usage,
            by: UsageGrouping::Agent,
            limit: 50,
        };
        let json = serde_json::to_value(&query).expect("serializes");
        assert_eq!(json["view"], "usage");
        assert_eq!(json["by"], "agent");
        assert_eq!(
            serde_json::from_value::<BrokerQuery>(json).expect("round trips"),
            query
        );
        assert_eq!(UsageGrouping::Agent.column(), "actor_id");
        assert_eq!(UsageGrouping::default().column(), "provider");
    }

    #[test]
    fn the_error_classification_survives_display() {
        let error = QueryError::invalid("nope");
        assert_eq!(error.code(), "invalid-input");
        assert_eq!(error.to_string(), "invalid-input: nope");
        assert_eq!(QueryError::upstream("nope").code(), "upstream-failure");
    }
}
