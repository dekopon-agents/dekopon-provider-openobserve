//! A bounded, read-only OpenObserve client for Dekopon: three command words over five capabilities.
//!
//! Goal 2 says everything that happened is in the operator's telemetry store. This component is how
//! an owner grants a model a bounded read of it — an ordinary `dekopon:http@1.1.0` client with a
//! broker-injected, DRN-bound Basic credential, no endpoint of its own, and no authority it did not
//! receive. It replaces something that was deleted rather than adding something new: `dekopon-run
//! session list | show | replay` read sessions back from OpenObserve and went with the runner in
//! 0.13.0. That client belonged out of tree, and this is where it lands.
//!
//! The component accepts no credential and sets no `authorization` header. Injection happens inside
//! the broker's native HTTP engine, for destinations inside the credential's binding, where no
//! guest can observe it; the host rejects an `authorization` header from a guest by construction
//! rather than overwriting it.
//!
//! Two host imports, both narrow. `dekopon:http/client@1.1.0` is the only way out. `dekopon:clock`
//! is read exactly once per `invoke`, because every statement carries an absolute
//! `start_time`/`end_time` in microseconds and a component has no clock of its own; it is readable
//! during `invoke` only, and the broker traps a component that reads it from `describe` or
//! `run-command`.
//!
//! Unlike dekopon's own crates this guest cannot `#![forbid(unsafe_code)]`: the generated component
//! bindings contain `unsafe` by construction. No hand-written code in this component is unsafe, and
//! `scripts/validate.sh` fails the build if any appears.

use dekopon_otel_query_core::query::{
    AgentStatsQuery, BrokerQuery, Query, SearchQuery, TraceQuery,
};
use dekopon_otel_query_core::{Backend, Capabilities, QueryError};
use dekopon_provider_http::{HttpError, Request, Response};
use dekopon_provider_sdk::{CapabilityId, CommandRun, Provider, ProviderError, ProviderManifest};
use serde_json::Value;

mod backend;
mod manifest;
mod wire;

/// The provider id, which every capability id is prefixed with.
pub(crate) const PROVIDER_ID: &str = "openobserve";
/// The raw-search command word, named after the backend.
///
/// #250's Backends section decided this: command words are global per broker and a duplicate fails
/// startup, so naming the raw word after the store lets an operator load OpenObserve for dekopon's
/// own telemetry and Quickwit for someone else's, side by side. `agent` and `broker` stay neutral
/// and are claimed only by whichever provider speaks dekopon's record schema.
pub(crate) const RAW_WORD: &str = "openobserve";
/// The neutral word for an agent's own numbers.
pub(crate) const AGENT_WORD: &str = dekopon_otel_query_core::grammar::AGENT_WORD;
/// The neutral word for the fleet view.
pub(crate) const BROKER_WORD: &str = dekopon_otel_query_core::grammar::BROKER_WORD;

const ABOUT: &str = "Query an OpenObserve telemetry store: raw SQL, or one trace's spans";

mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

/// The five capability ids this provider declares.
pub(crate) fn capabilities() -> Capabilities {
    Capabilities::for_provider(PROVIDER_ID)
}

/// The provider.
struct OpenObserveProvider;

impl Provider for OpenObserveProvider {
    fn manifest() -> ProviderManifest {
        manifest::manifest()
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        // One clock read per invocation, taken here rather than inside the planner: every statement
        // in one `agent stats` must bound the same window, and a planner that read the clock per
        // step would widen it between the sessions query and the turns query.
        invoke_with(
            capability,
            input,
            dekopon_provider_clock::now_unix_millis().saturating_mul(1_000),
            dekopon_provider_http::send,
        )
    }

    fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
        dispatch(&capabilities(), argv, stdin)
    }
}

/// Routes one argv to the command word that owns it.
///
/// The guest is never told which word was typed: `run-command` takes `argv` and `stdin` and nothing
/// else, and the broker host resolves the provider by the word before handing the rest over. The
/// three vocabularies are disjoint, so the action recovers the word; a bare `--help`, `--version`,
/// or an empty argv cannot, and renders one overview page naming all three words instead.
fn dispatch(
    capabilities: &Capabilities,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<CommandRun, ProviderError> {
    use dekopon_otel_query_core::grammar::{
        Route, overview, route, run_agent, run_broker, run_raw,
    };
    match route(argv) {
        Route::Raw => run_raw(RAW_WORD, ABOUT, capabilities, argv, stdin),
        Route::Agent => run_agent(capabilities, argv, stdin),
        Route::Broker => run_broker(capabilities, argv, stdin),
        Route::Overview => {
            let page = overview(RAW_WORD, ABOUT);
            match argv.first().map(String::as_str) {
                Some("--version" | "-V") => Ok(CommandRun::rendered(
                    format!("{RAW_WORD} {}\n", env!("CARGO_PKG_VERSION")),
                    0,
                )),
                Some("--help" | "-h") => Ok(CommandRun::rendered(page, 0)),
                // An empty argv or an unknown action is a usage error whose text is the overview,
                // as the upstream tool's would be: nothing was asked, so nothing is proposed.
                _ => Ok(CommandRun::rendered_error(page, 2)),
            }
        }
    }
}

/// The one place a capability becomes requests, with the clock and the transport injected.
///
/// Taking both as parameters is what lets every test assert the exact bytes of every request and
/// the exact projection of every response with no network, no host, and no wall clock: the seam is
/// the one the component uses, so what the tests exercise is what ships.
fn invoke_with<F>(
    capability: &CapabilityId,
    input: Value,
    now_us: u64,
    send: F,
) -> Result<Value, ProviderError>
where
    F: FnMut(Request) -> Result<Response, HttpError>,
{
    let mut query = parse(capability, input).map_err(provider_error)?;
    query.validate().map_err(provider_error)?;
    let rendering = query.scope().format;
    let answer = dekopon_otel_query_core::run(&backend::OpenObserve::at(now_us), &query, send)
        .map_err(provider_error)?;
    Ok(dekopon_otel_query_core::output::format(answer, rendering))
}

/// Parses the input object into the typed query its capability names.
fn parse(capability: &CapabilityId, input: Value) -> Result<Query, QueryError> {
    let ids = capabilities();
    let id = capability.as_str();
    let invalid = |error: serde_json::Error| QueryError::invalid(error.to_string());
    if id == ids.search.as_str() {
        return serde_json::from_value::<SearchQuery>(input)
            .map(Query::Search)
            .map_err(invalid);
    }
    if id == ids.trace.as_str() {
        return serde_json::from_value::<TraceQuery>(input)
            .map(Query::Trace)
            .map_err(invalid);
    }
    if id == ids.agent_stats.as_str() {
        return serde_json::from_value::<AgentStatsQuery>(input)
            .map(Query::AgentStats)
            .map_err(invalid);
    }
    if id == ids.broker_providers.as_str() || id == ids.broker_usage.as_str() {
        return serde_json::from_value::<BrokerQuery>(input)
            .map(Query::Broker)
            .map_err(invalid);
    }
    Err(QueryError::invalid(format!(
        "{id} is not a capability this provider declares"
    )))
}

fn provider_error(error: QueryError) -> ProviderError {
    ProviderError::new(error.code(), error.message().to_owned())
}

// The manual `Backend` import keeps `plan`/`fold` in scope for the driver without re-exporting it.
const _: fn() = || {
    fn assert_backend<B: Backend>() {}
    assert_backend::<backend::OpenObserve>();
};

dekopon_provider_sdk::export_provider_with_cli!(OpenObserveProvider, bindings);

#[cfg(test)]
pub(crate) fn capability(value: &str) -> CapabilityId {
    value.parse().expect("valid capability fixture")
}

#[cfg(test)]
mod tests {
    use dekopon_provider_http::{HttpError, Request, Response};
    use dekopon_provider_sdk::{CommandRun, Provider};
    use serde_json::{Value, json};

    use super::{OpenObserveProvider, capabilities, capability, dispatch, invoke_with};

    /// The clock reading every fixture is resolved against: 2026-09-12T00:00:00Z.
    const NOW_US: u64 = 1_789_084_800_000_000;

    const AGENT_SESSIONS: &str = include_str!("../tests/fixtures/agent-sessions.json");
    const AGENT_TURNS: &str = include_str!("../tests/fixtures/agent-turns.json");
    const AGENT_DECISIONS: &str = include_str!("../tests/fixtures/agent-decisions.json");
    const SEARCH_TRACES: &str = include_str!("../tests/fixtures/search-traces.json");
    const SEARCH_EMPTY: &str = include_str!("../tests/fixtures/search-empty.json");
    const SEARCH_401: &str = include_str!("../tests/fixtures/search-401.json");
    const SEARCH_400: &str = include_str!("../tests/fixtures/search-400-sql-error.json");
    const BROKER_BOOT: &str = include_str!("../tests/fixtures/broker-boot.json");
    const BROKER_LOADED: &str = include_str!("../tests/fixtures/broker-loaded-providers.json");
    const BROKER_USAGE: &str = include_str!("../tests/fixtures/broker-usage.json");

    /// Every request one scripted run made, in order.
    type Recorded = std::rc::Rc<std::cell::RefCell<Vec<Request>>>;

    /// A scripted transport: the recorded answers in order, with every request captured.
    fn scripted(
        steps: &[(u16, &'static str)],
    ) -> (
        impl FnMut(Request) -> Result<Response, HttpError> + use<>,
        Recorded,
    ) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = seen.clone();
        let steps: Vec<(u16, String)> = steps
            .iter()
            .map(|(status, body)| (*status, (*body).to_owned()))
            .collect();
        let mut index = 0_usize;
        let transport = move |request: Request| {
            recorded.borrow_mut().push(request);
            let (status, body) = steps
                .get(index)
                .unwrap_or_else(|| panic!("no scripted answer for request {index}"))
                .clone();
            index += 1;
            Ok(Response {
                status,
                headers: Vec::new(),
                body: body.into_bytes(),
            })
        };
        (transport, seen)
    }

    fn body(request: &Request) -> Value {
        serde_json::from_slice(&request.body).expect("a JSON request body")
    }

    /// The WIT in this repository is a mirror. Nothing shared keeps it honest out of tree, so the
    /// crates' own copies are the reference — in CI against the pinned release, and here against
    /// the compiled constants.
    #[test]
    fn mirrored_wit_exactly_matches_the_pinned_crates() {
        assert_eq!(
            include_str!("../../../wit/deps/provider.wit"),
            dekopon_provider_sdk::PROVIDER_WIT
        );
        assert_eq!(
            include_str!("../../../wit/deps/http.wit"),
            dekopon_provider_http::HTTP_WIT
        );
    }

    /// The manifest, pinned byte for byte. Effect and risk have to match the broker's constraint
    /// sets and the gateway's catalog exactly, and a snapshot is how a change to either becomes a
    /// diff a reviewer sees.
    #[test]
    fn manifest_snapshot() {
        let actual = format!(
            "{}\n",
            serde_json::to_string_pretty(&OpenObserveProvider::manifest())
                .expect("manifest serializes")
        );
        let expected = include_str!("../tests/fixtures/manifest.json");
        assert_eq!(actual, expected);
        let decoded: Value = serde_json::from_str(expected).expect("the snapshot is JSON");
        assert_eq!(decoded["capabilities"].as_array().expect("array").len(), 5);
        assert_eq!(
            decoded["commandWords"],
            json!(["openobserve", "agent", "broker"])
        );
    }

    /// One statement, one POST, with the window resolved against the injected clock.
    #[test]
    fn a_raw_statement_is_one_post_and_its_rows_come_back_fitted() {
        let (transport, seen) = scripted(&[(200, SEARCH_TRACES)]);
        let output = invoke_with(
            &capability("openobserve.search"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 3_600,
                "signal": "traces",
                "sql": "SELECT trace_id, operation_name FROM \"dekopon\"",
                "limit": 2
            }),
            NOW_US,
            transport,
        )
        .expect("rows");

        let requests = seen.borrow();
        assert_eq!(requests.len(), 1, "one statement is one request");
        assert_eq!(
            requests[0].uri,
            "http://rpi.lan/openobserve/api/default/_search?type=traces"
        );
        assert_eq!(
            body(&requests[0])["query"]["start_time"],
            NOW_US - 3_600_000_000
        );
        assert_eq!(body(&requests[0])["query"]["end_time"], NOW_US);
        assert_eq!(body(&requests[0])["query"]["size"], 2);
        assert_eq!(output["returned"], 2);
        assert_eq!(output["total"], 1_873);
        assert_eq!(output["truncated"], true);
        assert_eq!(output["omittedRows"], 1_871);
    }

    /// The two formats, from one recorded answer: JSON pipes, the table reads, and both say the
    /// store had 1873 rows and this envelope carries two.
    #[test]
    fn both_formats_come_out_of_the_same_answer_with_the_same_marker() {
        let (transport, _) = scripted(&[(200, SEARCH_TRACES)]);
        let input = json!({
            "url": "http://rpi.lan/openobserve",
            "sinceSeconds": 3_600,
            "signal": "traces",
            "sql": "SELECT trace_id, operation_name FROM \"dekopon\"",
            "limit": 2
        });
        let json_output = invoke_with(
            &capability("openobserve.search"),
            input.clone(),
            NOW_US,
            transport,
        )
        .expect("rows");
        assert_eq!(json_output["rows"][0]["operation_name"], "gateway.session");
        assert_eq!(json_output["truncated"], true);
        assert_eq!(json_output["omittedRows"], 1_871);

        let (transport, _) = scripted(&[(200, SEARCH_TRACES)]);
        let mut table_input = input;
        table_input["format"] = json!("table");
        let table = invoke_with(
            &capability("openobserve.search"),
            table_input,
            NOW_US,
            transport,
        )
        .expect("a table");
        let text = table
            .as_str()
            .expect("a table is a string the shell prints verbatim");
        // Column order is the parsed object's key order, which `serde_json`'s default map makes
        // alphabetical — stable across runs, which is what a model reading two answers needs.
        let header = text.lines().next().expect("a header");
        assert_eq!(
            header.split_whitespace().collect::<Vec<_>>(),
            ["operation_name", "trace_id"]
        );
        assert!(text.contains("gateway.session"), "{text}");
        assert_eq!(
            text.lines().last(),
            Some("-- 2 of 1873 rows; truncated, 1871 omitted"),
            "{text}"
        );
    }

    /// `agent stats` renders as a two-column table too, with the same numbers.
    #[test]
    fn agent_stats_renders_as_a_table_of_flattened_paths() {
        let (transport, _) = scripted(&[
            (200, AGENT_SESSIONS),
            (200, AGENT_TURNS),
            (200, AGENT_DECISIONS),
        ]);
        let table = invoke_with(
            &capability("openobserve.agent-stats"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 86_400,
                "agent": "reviewer",
                "format": "table"
            }),
            NOW_US,
            transport,
        )
        .expect("a table");
        let text = table.as_str().expect("a string");
        assert!(text.contains("turns.durationMs.p95"), "{text}");
        assert!(text.contains("9800"), "{text}");
        assert!(text.contains("tokens.cachedInput"), "{text}");
        assert!(text.contains("capabilities.gh.pull-request.read"), "{text}");
    }

    /// `agent stats` is three statements: the agent's sessions, the turns beneath them, and the
    /// broker decisions it made. The second is built from the first answer.
    #[test]
    fn agent_stats_joins_sessions_to_turns_by_trace_id() {
        let (transport, seen) = scripted(&[
            (200, AGENT_SESSIONS),
            (200, AGENT_TURNS),
            (200, AGENT_DECISIONS),
        ]);
        let output = invoke_with(
            &capability("openobserve.agent-stats"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 86_400,
                "agent": "reviewer"
            }),
            NOW_US,
            transport,
        )
        .expect("stats");

        let requests = seen.borrow();
        assert_eq!(requests.len(), 3);
        let sessions = body(&requests[0])["query"]["sql"]
            .as_str()
            .expect("sql")
            .to_owned();
        assert!(
            sessions.contains("operation_name = 'gateway.session'"),
            "{sessions}"
        );
        assert!(sessions.contains("agent = 'reviewer'"), "{sessions}");
        let turns = body(&requests[1])["query"]["sql"]
            .as_str()
            .expect("sql")
            .to_owned();
        assert!(
            turns.contains("operation_name = 'prompt.model_turn'"),
            "{turns}"
        );
        assert!(
            turns.contains("'0af7651916cd43dd8448eb211c80319c'")
                && turns.contains("'1bf7651916cd43dd8448eb211c80319d'"),
            "{turns}"
        );
        assert!(requests[1].uri.ends_with("type=traces"));
        let decisions = body(&requests[2])["query"]["sql"]
            .as_str()
            .expect("sql")
            .to_owned();
        assert!(
            decisions.contains("audit_event = 'broker.decision'"),
            "{decisions}"
        );
        assert!(
            requests[2].uri.ends_with("type=logs"),
            "decisions are log records"
        );

        assert_eq!(output["agent"], "reviewer");
        assert_eq!(output["sessions"], 2);
        assert_eq!(output["turns"]["count"], 41);
        // The store reports span durations in microseconds; the shape is milliseconds.
        assert_eq!(output["turns"]["durationMs"]["p50"], 2_140);
        assert_eq!(output["turns"]["durationMs"]["p95"], 9_800);
        assert_eq!(output["tokens"]["input"], 183_220);
        assert_eq!(output["tokens"]["cachedInput"], 121_004);
        assert_eq!(output["tokens"]["total"], 205_630);
        assert_eq!(output["capabilities"]["gh.pull-request.read"], 17);
        assert_eq!(output["capabilities"]["gh.pull-request.files"], 12);
        assert_eq!(output["denials"]["policy-denied"], 1);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["window"]["untilUs"], NOW_US);
        assert_eq!(output["window"]["sinceUs"], NOW_US - 86_400_000_000);
    }

    /// An agent with no sessions in the window costs two requests, not three: an `IN ()` is not a
    /// statement, and a denial does not need a session to exist.
    #[test]
    fn an_agent_with_no_sessions_still_reports_its_denials() {
        let (transport, seen) = scripted(&[(200, SEARCH_EMPTY), (200, AGENT_DECISIONS)]);
        let output = invoke_with(
            &capability("openobserve.agent-stats"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 86_400,
                "agent": "newcomer"
            }),
            NOW_US,
            transport,
        )
        .expect("stats");
        assert_eq!(seen.borrow().len(), 2);
        assert_eq!(output["sessions"], 0);
        assert_eq!(output["turns"]["count"], 0);
        assert_eq!(output["tokens"]["total"], 0);
        assert_eq!(output["denials"]["policy-denied"], 1);
    }

    /// `broker providers` finds the last boot, then reads what was announced after it.
    #[test]
    fn broker_providers_anchors_on_the_last_boot_in_the_window() {
        let (transport, seen) = scripted(&[(200, BROKER_BOOT), (200, BROKER_LOADED)]);
        let output = invoke_with(
            &capability("openobserve.broker-providers"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 86_400,
                "view": "providers"
            }),
            NOW_US,
            transport,
        )
        .expect("providers");

        let requests = seen.borrow();
        assert_eq!(requests.len(), 2);
        assert!(
            body(&requests[0])["query"]["sql"]
                .as_str()
                .expect("sql")
                .contains("event = 'broker_started'")
        );
        // The second statement's window starts at the boot, not at --since.
        assert_eq!(
            body(&requests[1])["query"]["start_time"],
            1_789_060_800_000_000_u64
        );
        assert_eq!(output["bootedAtUs"], 1_789_060_800_000_000_u64);
        assert_eq!(output["providers"].as_array().expect("array").len(), 2);
        assert_eq!(output["providers"][0]["provider"], "gh");
        assert_eq!(output["truncated"], false);
        // `path` is in the record and must never be in the answer.
        assert!(
            !serde_json::to_string(&output)
                .expect("serializes")
                .contains("\"path\"")
        );
    }

    /// A broker that has not booted inside the window costs one request and says so.
    #[test]
    fn broker_providers_with_no_boot_in_the_window_is_one_request_and_a_null() {
        let (transport, seen) = scripted(&[(200, SEARCH_EMPTY)]);
        let output = invoke_with(
            &capability("openobserve.broker-providers"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 3_600,
                "view": "providers"
            }),
            NOW_US,
            transport,
        )
        .expect("providers");
        assert_eq!(seen.borrow().len(), 1);
        assert!(output["bootedAtUs"].is_null());
        assert_eq!(output["providers"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn broker_usage_groups_on_the_column_the_input_named() {
        let (transport, seen) = scripted(&[(200, BROKER_USAGE)]);
        let output = invoke_with(
            &capability("openobserve.broker-usage"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 86_400,
                "view": "usage",
                "by": "agent"
            }),
            NOW_US,
            transport,
        )
        .expect("usage");
        assert!(
            body(&seen.borrow()[0])["query"]["sql"]
                .as_str()
                .expect("sql")
                .contains("SELECT actor_id AS bucket")
        );
        assert_eq!(output["returned"], 2);
        assert_eq!(output["rows"][0]["bucket"], "reviewer");
    }

    /// One trace's spans, projected. `events` carries the conversation and is never selected.
    #[test]
    fn a_trace_returns_a_projected_span_skeleton() {
        let (transport, seen) = scripted(&[(200, SEARCH_TRACES)]);
        invoke_with(
            &capability("openobserve.trace"),
            json!({
                "url": "http://rpi.lan/openobserve",
                "sinceSeconds": 3_600,
                "traceId": "0af7651916cd43dd8448eb211c80319c"
            }),
            NOW_US,
            transport,
        )
        .expect("spans");
        let sql = body(&seen.borrow()[0])["query"]["sql"]
            .as_str()
            .expect("sql")
            .to_owned();
        assert!(
            sql.contains("trace_id = '0af7651916cd43dd8448eb211c80319c'"),
            "{sql}"
        );
        assert!(!sql.contains("events"), "{sql}");
        assert!(!sql.contains('*'), "{sql}");
    }

    /// A refused credential and a rejected statement are classified, not passed through raw.
    #[test]
    fn upstream_refusals_are_classified() {
        for (status, fixture, needle) in [
            (401_u16, SEARCH_401, "credential"),
            (400, SEARCH_400, "audit_event"),
        ] {
            let (transport, _) = scripted(&[(status, fixture)]);
            let error = invoke_with(
                &capability("openobserve.search"),
                json!({
                    "url": "http://rpi.lan/openobserve",
                    "sinceSeconds": 3_600,
                    "signal": "traces",
                    "sql": "SELECT 1"
                }),
                NOW_US,
                transport,
            )
            .expect_err("refused");
            assert_eq!(error.code(), "upstream-failure", "{status}");
            assert!(
                error.message().contains(needle),
                "{status}: {}",
                error.message()
            );
        }
    }

    /// Invalid input never reaches the network: the transport asserts it was not called.
    #[test]
    fn invalid_input_costs_no_request() {
        for (id, input) in [
            (
                "openobserve.search",
                json!({"url": "http://rpi.lan/openobserve", "sinceSeconds": 3_600, "signal": "traces", "sql": "DELETE FROM \"dekopon\""}),
            ),
            (
                "openobserve.search",
                json!({"url": "rpi.lan", "sinceSeconds": 3_600, "signal": "traces", "sql": "SELECT 1"}),
            ),
            (
                "openobserve.trace",
                json!({"url": "http://rpi.lan/openobserve", "sinceSeconds": 3_600, "traceId": "nope"}),
            ),
            (
                "openobserve.agent-stats",
                json!({"url": "http://rpi.lan/openobserve", "sinceSeconds": 86_400, "agent": "review er"}),
            ),
            (
                "openobserve.search",
                json!({"url": "http://rpi.lan/openobserve", "sinceSeconds": 3_600, "signal": "traces", "sql": "SELECT 1", "bogus": 1}),
            ),
            (
                "openobserve.upscale",
                json!({"url": "http://rpi.lan/openobserve", "sinceSeconds": 3_600}),
            ),
        ] {
            let error = invoke_with(&capability(id), input.clone(), NOW_US, |_request| {
                panic!("no request may be sent for {input}")
            })
            .expect_err("refused");
            assert_eq!(error.code(), "invalid-input", "{input}");
        }
    }

    /// The word is not on the argv, so the action recovers it — and the one argv that cannot,
    /// a bare `--help`, renders one overview naming all three words rather than one word's page.
    #[test]
    fn the_action_recovers_the_word_the_host_did_not_pass() {
        let ids = capabilities();
        for (action, usage) in [
            ("sql", "Usage: openobserve sql"),
            ("search", "Usage: openobserve search"),
            ("trace", "Usage: openobserve trace"),
            ("stats", "Usage: agent stats"),
            ("providers", "Usage: broker providers"),
            ("usage", "Usage: broker usage"),
            ("denials", "Usage: broker denials"),
        ] {
            let run =
                dispatch(&ids, &[action.to_owned(), "--help".to_owned()], None).expect("rendered");
            let CommandRun::Rendered { stdout, status, .. } = run else {
                panic!("{action}: expected rendered help");
            };
            assert_eq!(status, 0, "{action}");
            assert!(stdout.contains(usage), "{action}: {stdout}");
        }

        let CommandRun::Rendered { stdout, status, .. } =
            dispatch(&ids, &["--help".to_owned()], None).expect("rendered")
        else {
            panic!("expected an overview");
        };
        assert_eq!(status, 0);
        for line in [
            "openobserve sql",
            "openobserve search",
            "openobserve trace",
            "agent stats",
            "broker providers",
            "broker usage",
            "broker denials",
        ] {
            assert!(stdout.contains(line), "{stdout}");
        }
        assert!(stdout.contains("30d"), "{stdout}");
        assert!(stdout.contains("audit_event"), "{stdout}");

        // An empty argv and an unknown action are usage errors carrying the same page.
        for argv in [vec![], vec!["quickwit".to_owned()]] {
            let CommandRun::Rendered {
                stdout,
                stderr,
                status,
            } = dispatch(&ids, &argv, None).expect("rendered")
            else {
                panic!("expected the overview, got a proposal for {argv:?}");
            };
            assert_eq!(status, 2, "{argv:?}");
            assert!(stdout.is_empty(), "{argv:?}");
            assert!(stderr.contains("openobserve sql"), "{argv:?}");
        }

        let CommandRun::Rendered { stdout, status, .. } =
            dispatch(&ids, &["--version".to_owned()], None).expect("rendered")
        else {
            panic!("expected a version");
        };
        assert_eq!(status, 0);
        assert_eq!(
            stdout,
            format!("openobserve {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    /// `run_command` is pure by contract: it must not reach a host import, so it must not read the
    /// clock. A proposal carries no resolved window, only the seconds the caller asked for.
    #[test]
    fn a_proposal_carries_relative_seconds_and_never_an_absolute_window() {
        let run = dispatch(
            &capabilities(),
            &[
                "stats".to_owned(),
                "--url".to_owned(),
                "http://rpi.lan/openobserve".to_owned(),
                "--agent".to_owned(),
                "reviewer".to_owned(),
                "--since".to_owned(),
                "24h".to_owned(),
            ],
            None,
        )
        .expect("a proposal");
        let CommandRun::Proposal(invocation) = run else {
            panic!("expected a proposal");
        };
        assert_eq!(invocation.input["sinceSeconds"], 86_400);
        assert!(invocation.input.get("startTime").is_none());
        assert!(invocation.input.get("endTime").is_none());
    }
}
