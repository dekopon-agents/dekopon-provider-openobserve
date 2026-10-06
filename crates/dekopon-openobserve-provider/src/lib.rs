//! Read-only OpenObserve telemetry through the typed Dekopon provider SDK.
use std::fmt;
use std::io::Write;

use dekopon_otel_query_core::grammar::Commands;
use dekopon_otel_query_core::query::{
    AgentStatsQuery, BrokerQuery, BrokerView, Query, QueryError, TraceQuery,
};
use dekopon_provider_sdk::provider::{
    Capability, Clock, Code, Failure, Http, HttpError, Proposal, Provider, Request, Response,
    Settings, Stdout, Usage,
};
use dekopon_provider_sdk::schemars::{JsonSchema, Schema, SchemaGenerator};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod backend;
mod manifest;
mod wire;

/// The OpenObserve provider.
pub struct OpenObserveProvider;
/// One projected trace read.
pub struct Trace;
/// One fixed-window agent aggregate.
pub struct AgentStats;
/// One broker boot and provider row read.
pub struct BrokerProviders;
/// One fixed-window broker aggregate.
pub struct BrokerUsage;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Owner-only connection parameters, loaded after authorization.
pub struct OwnerSettings {
    url: String,
    org: String,
    stream: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// A projected trace, without caller-controlled destination or SQL.
pub struct TraceInput {
    trace_id: String,
    since_seconds: u64,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    format: dekopon_otel_query_core::query::Format,
    #[serde(default = "default_output")]
    max_output_bytes: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// One agent, over an immutable five-minute window.
pub struct AgentInput {
    agent: String,
    #[serde(default)]
    format: dekopon_otel_query_core::query::Format,
    #[serde(default = "default_output")]
    max_output_bytes: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Provider rows, bounded to 24 hours.
pub struct ProvidersInput {
    since_seconds: u64,
    view: BrokerView,
    #[serde(default)]
    format: dekopon_otel_query_core::query::Format,
    #[serde(default = "default_output")]
    max_output_bytes: usize,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
/// Usage or denials in a fixed five-minute slice.
pub struct UsageInput {
    view: BrokerView,
    #[serde(default)]
    by: dekopon_otel_query_core::query::UsageGrouping,
    #[serde(default = "default_groups")]
    limit: u32,
    #[serde(default)]
    format: dekopon_otel_query_core::query::Format,
    #[serde(default = "default_output")]
    max_output_bytes: usize,
}
fn default_limit() -> u32 {
    100
}
fn default_groups() -> u32 {
    20
}
fn default_output() -> usize {
    65_536
}

// Schema and deserializer are kept together: both are closed and validate the same wire keys.
macro_rules! schema {
    ($input:ty, $name:literal, $value:expr) => {
        impl JsonSchema for $input {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                serde_json::from_value($value).expect("static closed input schema")
            }
        }
    };
}
schema!(TraceInput, "TraceInput", manifest::trace_schema());
schema!(AgentInput, "AgentInput", manifest::agent_stats_schema());
schema!(
    ProvidersInput,
    "ProvidersInput",
    manifest::broker_schema(&["providers"], false)
);
schema!(
    UsageInput,
    "UsageInput",
    manifest::broker_schema(&["usage", "denials"], true)
);

impl Provider for OpenObserveProvider {
    const ID: &'static str = "openobserve";
    const COMMAND_WORDS: &'static [&'static str] = &["openobserve", "agent", "broker"];
    const DESCRIPTION: &'static str =
        "Reads configured OpenObserve telemetry with bounded generated queries";
    type Args = Commands;
    type Capabilities = (Trace, AgentStats, BrokerProviders, BrokerUsage);
    fn propose(args: Self::Args, _stdin_piped: bool) -> Result<Proposal<Self>, Usage> {
        let (name, input) =
            dekopon_otel_query_core::grammar::proposal(args.action).map_err(Usage::new)?;
        match name {
            "trace" => typed::<TraceInput, Trace>(input),
            "agent-stats" => typed::<AgentInput, AgentStats>(input),
            "broker-providers" => typed::<ProvidersInput, BrokerProviders>(input),
            "broker-usage" => typed::<UsageInput, BrokerUsage>(input),
            _ => Err(Usage::new("unknown action")),
        }
    }
}
fn typed<I, C>(input: Value) -> Result<Proposal<OpenObserveProvider>, Usage>
where
    C: Capability<Provider = OpenObserveProvider, Input = I>,
    I: serde::de::DeserializeOwned + Serialize + JsonSchema,
{
    let input = serde_json::from_value(input).map_err(|_| Usage::new("invalid command input"))?;
    Ok(Proposal::to::<C>(input))
}

/// Sanitized provider failure; no upstream body or owner setting is displayed.
#[derive(Debug)]
pub struct ProviderError {
    code: &'static str,
    message: String,
}
impl From<QueryError> for ProviderError {
    fn from(error: QueryError) -> Self {
        Self {
            code: error.code(),
            message: error.message().to_owned(),
        }
    }
}
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Failure for ProviderError {
    fn code(&self) -> Code {
        match self.code {
            "invalid-input" => Code::INVALID_INPUT,
            "upstream-failure" => Code::new("upstream-failure"),
            _ => Code::new("output-closed"),
        }
    }
}
fn emit(value: Value, out: &mut Stdout) -> Result<(), ProviderError> {
    let bytes = match value {
        Value::String(text) => text.into_bytes(),
        value => serde_json::to_vec(&value).map_err(|_| ProviderError {
            code: "output-closed",
            message: "could not serialize output".into(),
        })?,
    };
    out.write_all(&bytes)
        .and_then(|()| {
            if bytes.ends_with(b"\n") {
                Ok(())
            } else {
                out.write_all(b"\n")
            }
        })
        .map_err(|_| ProviderError {
            code: "output-closed",
            message: "stdout's reader has gone".into(),
        })
}
fn invoke_with<F>(
    name: &str,
    input: Value,
    settings: &OwnerSettings,
    now_us: u64,
    send: F,
) -> Result<Value, ProviderError>
where
    F: FnMut(Request) -> Result<Response, HttpError>,
{
    let mut input = input
        .as_object()
        .cloned()
        .ok_or_else(|| QueryError::invalid("expected an input object"))?;
    if ["url", "org", "stream"]
        .iter()
        .any(|key| input.contains_key(*key))
    {
        return Err(QueryError::invalid("store destination fields are owner-configured").into());
    }
    input.insert("url".into(), json!(settings.url));
    input.insert("org".into(), json!(settings.org));
    input.insert("stream".into(), json!(settings.stream));
    let value = Value::Object(input);
    let invalid = |_error: serde_json::Error| {
        QueryError::invalid("input does not match the closed query schema")
    };
    let mut query = match name {
        "trace" => Query::Trace(serde_json::from_value::<TraceQuery>(value).map_err(invalid)?),
        "agent-stats" => {
            Query::AgentStats(serde_json::from_value::<AgentStatsQuery>(value).map_err(invalid)?)
        }
        "broker-providers" | "broker-usage" => {
            let query: BrokerQuery = serde_json::from_value(value).map_err(invalid)?;
            if (name == "broker-providers") != (query.view == BrokerView::Providers) {
                return Err(
                    QueryError::invalid("broker capability does not authorize this view").into(),
                );
            }
            Query::Broker(query)
        }
        _ => return Err(QueryError::invalid("unknown capability").into()),
    };
    query.validate()?;
    let format = query.scope().format;
    let answer = dekopon_otel_query_core::run(&backend::OpenObserve::at(now_us), &query, send)?;
    Ok(dekopon_otel_query_core::output::format(answer, format))
}
macro_rules! capability {
    ($ty:ty, $name:literal, $description:literal, $input:ty) => {
        impl Capability for $ty {
            type Provider = OpenObserveProvider;
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = $description;
            const EFFECT: EffectKind = EffectKind::ReadOnly;
            const RISK: RiskLevel = RiskLevel::Low;
            type Input = $input;
            type Needs = (Http, Clock, Settings<OwnerSettings>);
            type Error = ProviderError;
            fn run(
                input: Self::Input,
                (http, clock, settings): Self::Needs,
                out: &mut Stdout,
            ) -> Result<(), Self::Error> {
                // The broker clock is consulted exactly once, after authorization and settings load.
                let now_us = clock.now_unix_millis().saturating_mul(1_000);
                let value = serde_json::to_value(input)
                    .map_err(|_| QueryError::invalid("invalid input"))?;
                emit(
                    invoke_with($name, value, &settings.into_inner(), now_us, |request| {
                        http.send(request)
                    })?,
                    out,
                )
            }
        }
    };
}
capability!(
    Trace,
    "trace",
    "Projected trace spans without conversation events",
    TraceInput
);
capability!(
    AgentStats,
    "agent-stats",
    "One agent's fixed five-minute sessions, turns, and decisions",
    AgentInput
);
capability!(
    BrokerProviders,
    "broker-providers",
    "Broker boot and loaded providers within 24 hours",
    ProvidersInput
);
capability!(
    BrokerUsage,
    "broker-usage",
    "Five-minute broker usage or denials without per-actor groups",
    UsageInput
);

#[cfg(target_arch = "wasm32")]
mod export {
    dekopon_provider_sdk::export!(super::OpenObserveProvider);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> OwnerSettings {
        OwnerSettings {
            url: "https://rpi.lan/openobserve".into(),
            org: "default".into(),
            stream: "dekopon".into(),
        }
    }
    const NOW: u64 = 1_789_084_800_000_000;

    #[test]
    fn native_stdio_emits_one_terminal_newline_for_json_and_table() {
        use dekopon_provider_sdk::provider::{
            NativeStdio, Port, StreamedRequest, StreamedResponse, invoke_native, with_port,
        };
        use std::cell::RefCell;
        use std::io::{self, Write};
        use std::rc::Rc;

        struct FixturePort;
        impl Port for FixturePort {
            fn now_unix_millis(&mut self) -> u64 {
                NOW / 1_000
            }
            fn now_nanos(&mut self) -> u64 {
                unreachable!("no monotonic clock")
            }
            fn fill_random(&mut self, _: &mut [u8]) {
                unreachable!("no entropy")
            }
            fn settings(&mut self) -> Option<String> {
                Some(
                    r#"{"url":"https://rpi.lan/openobserve","org":"default","stream":"dekopon"}"#
                        .to_owned(),
                )
            }
            fn send(&mut self, _: Request) -> Result<Response, HttpError> {
                Ok(Response {
                    status: 200,
                    headers: vec![],
                    body: br#"{"hits":[],"total":0}"#.to_vec(),
                })
            }
            fn stream(&mut self, _: StreamedRequest<'_>) -> Result<StreamedResponse, HttpError> {
                unreachable!("buffered HTTP only")
            }
        }
        struct Buffer(Rc<RefCell<Vec<u8>>>);
        impl Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        for format in ["json", "table"] {
            let captured = Rc::new(RefCell::new(Vec::new()));
            let input = json!({"traceId":"0af7651916cd43dd8448eb211c80319c", "sinceSeconds":300, "format":format});
            let exit = with_port(FixturePort, || {
                invoke_native::<OpenObserveProvider>(
                    "openobserve.trace",
                    &input.to_string(),
                    NativeStdio {
                        stdin: None,
                        stdout: Box::new(Buffer(Rc::clone(&captured))),
                    },
                )
            });
            assert_eq!(exit.status, 0, "{}", exit.stderr);
            assert!(exit.stderr.is_empty());
            let bytes = captured.borrow();
            assert!(bytes.ends_with(b"\n"), "{format}");
            assert!(!bytes.ends_with(b"\n\n"), "{format}: {bytes:?}");
            let (value, _) = capture("trace", input, &[r#"{"hits":[],"total":0}"#]);
            let expected = match value.expect("answer") {
                Value::String(text) => text.into_bytes(),
                value => serde_json::to_vec(&value).unwrap(),
            };
            let mut expected = expected;
            if !expected.ends_with(b"\n") {
                expected.push(b'\n');
            }
            assert_eq!(*bytes, expected, "{format} exact emitted bytes");
        }
    }
    fn response(body: &str) -> Response {
        Response {
            status: 200,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }
    fn capture(
        name: &str,
        input: Value,
        answers: &[&str],
    ) -> (Result<Value, ProviderError>, Vec<Value>) {
        let mut bodies = vec![];
        let mut index = 0;
        let result = invoke_with(name, input, &settings(), NOW, |request| {
            bodies.push(serde_json::from_slice(&request.body).expect("JSON request"));
            let answer = answers[index];
            index += 1;
            Ok(response(answer))
        });
        (result, bodies)
    }
    #[test]
    fn help_uses_the_command_word_the_broker_routed() {
        use dekopon_provider_sdk::{CommandRunOutcome, provider};
        for (action, expected) in [
            ("trace", "Usage: openobserve trace"),
            ("stats", "Usage: agent stats"),
            ("providers", "Usage: broker providers"),
            ("usage", "Usage: broker usage"),
            ("denials", "Usage: broker denials"),
        ] {
            let argv = vec![action.to_owned(), "--help".to_owned()];
            let CommandRunOutcome::Rendered {
                stdout,
                stderr,
                status,
            } = provider::command::<OpenObserveProvider>(&argv, false)
            else {
                panic!("help must render for {action}");
            };
            assert_eq!(status, 0);
            assert!(stderr.is_empty());
            assert!(stdout.contains(expected), "{action}: {stdout}");
        }
    }

    #[test]
    fn cli_proposals_follow_the_same_closed_body_path_as_direct_invocation() {
        use dekopon_provider_sdk::{CommandRunOutcome, provider};
        let empty = r#"{"hits":[],"total":0}"#;
        for (argv, expected_capability, expected_size, expected_seconds) in [
            (
                vec![
                    "trace",
                    "--since",
                    "24h",
                    "0af7651916cd43dd8448eb211c80319c",
                ],
                "openobserve.trace",
                100,
                86400,
            ),
            (
                vec!["stats", "--agent", "reviewer"],
                "openobserve.agent-stats",
                50,
                300,
            ),
            (
                vec!["providers", "--since", "24h"],
                "openobserve.broker-providers",
                1,
                86400,
            ),
            (
                vec!["usage", "--by", "capability"],
                "openobserve.broker-usage",
                20,
                300,
            ),
            (vec!["denials"], "openobserve.broker-usage", 20, 300),
        ] {
            let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let CommandRunOutcome::Proposed {
                capability, input, ..
            } = provider::command::<OpenObserveProvider>(&argv, false)
            else {
                panic!("expected proposal for {argv:?}");
            };
            assert_eq!(capability.as_str(), expected_capability);
            let name = capability.as_str().strip_prefix("openobserve.").unwrap();
            let (_, bodies) = capture(name, input, &[empty, empty]);
            assert!(!bodies.is_empty());
            assert_eq!(bodies[0]["query"]["size"], expected_size);
            assert_eq!(bodies[0]["query"]["end_time"], NOW);
            assert_eq!(
                bodies[0]["query"]["start_time"],
                NOW - expected_seconds * 1_000_000
            );
        }
        for argv in [
            vec!["stats", "--since", "1h", "--agent", "reviewer"],
            vec!["usage", "--by", "agent"],
            vec!["providers", "--since", "86401s"],
        ] {
            let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!matches!(
                provider::command::<OpenObserveProvider>(&argv, false),
                CommandRunOutcome::Proposed { .. }
            ));
        }
    }

    #[test]
    fn fixed_aggregate_bodies_are_bounded_on_every_step() {
        let sessions = r#"{"hits":[{"trace_id":"0af7651916cd43dd8448eb211c80319c"}],"total":1}"#;
        let empty = r#"{"hits":[],"total":0}"#;
        for (name, input, answers, sizes) in [
            (
                "agent-stats",
                json!({"agent":"reviewer"}),
                vec![sessions, empty, empty],
                vec![50, 1, 50],
            ),
            (
                "agent-stats",
                json!({"agent":"reviewer"}),
                vec![empty, empty],
                vec![50, 50],
            ),
            (
                "broker-usage",
                json!({"view":"usage"}),
                vec![empty],
                vec![20],
            ),
            (
                "broker-usage",
                json!({"view":"denials"}),
                vec![empty],
                vec![20],
            ),
        ] {
            let (result, bodies) = capture(name, input, &answers);
            result.expect("result");
            assert_eq!(bodies.len(), sizes.len());
            for (body, size) in bodies.iter().zip(sizes) {
                assert_eq!(body["query"]["end_time"], NOW);
                assert_eq!(body["query"]["start_time"], NOW - 300_000_000);
                assert_eq!(body["query"]["size"], size);
                let sql = body["query"]["sql"].as_str().unwrap();
                assert!(!sql.contains("GROUP BY trace_id"));
                if name == "broker-usage" {
                    assert!(!sql.contains("actor_id"));
                }
            }
        }
    }
    #[test]
    fn boot_before_inside_and_after_window_end_never_widens_the_second_request() {
        let empty = r#"{"hits":[],"total":0}"#;
        for (boot, expected_start, count) in [
            (NOW - 90_000_000_000, Some(NOW - 86_400_000_000), 2),
            (NOW - 3_000_000, Some(NOW - 3_000_000), 2),
            (NOW + 1, None, 1),
        ] {
            let boot_row = format!(r#"{{"hits":[{{"_timestamp":{boot}}}],"total":1}}"#);
            let mut bodies = vec![];
            let mut index = 0;
            let result = invoke_with(
                "broker-providers",
                json!({"view":"providers", "sinceSeconds":86400}),
                &settings(),
                NOW,
                |request| {
                    bodies.push(serde_json::from_slice::<Value>(&request.body).unwrap());
                    let body = if index == 0 { boot_row.as_str() } else { empty };
                    index += 1;
                    Ok(response(body))
                },
            );
            result.expect("providers");
            assert_eq!(bodies.len(), count);
            assert_eq!(bodies[0]["query"]["size"], 1);
            if let Some(start) = expected_start {
                assert_eq!(bodies[1]["query"]["start_time"], start);
                assert_eq!(bodies[1]["query"]["end_time"], NOW);
                assert_eq!(bodies[1]["query"]["size"], 200);
            }
        }
    }

    #[test]
    fn hostile_oversized_sessions_fan_out_to_at_most_fifty_unique_valid_ids() {
        let rows: Vec<Value> = (0..200)
            .map(|i| json!({"trace_id": format!("{i:032x}")}))
            .collect();
        let sessions = json!({"hits": rows, "total": 200}).to_string();
        let empty = r#"{"hits":[],"total":0}"#;
        let mut sql = String::new();
        let mut count = 0;
        invoke_with(
            "agent-stats",
            json!({"agent":"reviewer"}),
            &settings(),
            NOW,
            |request| {
                if count == 1 {
                    sql = serde_json::from_slice::<Value>(&request.body).unwrap()["query"]["sql"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                }
                let body = if count == 0 { sessions.as_str() } else { empty };
                count += 1;
                Ok(response(body))
            },
        )
        .expect("bounded result");
        assert_eq!(count, 3);
        assert!(sql.contains("'00000000000000000000000000000031'"));
        assert!(!sql.contains("'00000000000000000000000000000032'"));
        assert!(sql.ends_with("LIMIT 1"));
    }

    #[test]
    fn mismatched_views_oversized_windows_and_aggregate_since_cost_zero_requests() {
        for (name, input) in [
            (
                "broker-providers",
                json!({"view":"usage", "sinceSeconds":300}),
            ),
            ("broker-usage", json!({"view":"providers"})),
            (
                "broker-usage",
                json!({"view":"denials", "sinceSeconds":300}),
            ),
            (
                "agent-stats",
                json!({"agent":"reviewer", "sinceSeconds":300}),
            ),
            (
                "trace",
                json!({"traceId":"0af7651916cd43dd8448eb211c80319c", "sinceSeconds":86401}),
            ),
            (
                "broker-providers",
                json!({"view":"providers", "sinceSeconds":86401}),
            ),
            ("broker-usage", json!({"view":"usage", "by":"agent"})),
        ] {
            let error = invoke_with(name, input, &settings(), NOW, |_request| {
                panic!("HTTP on invalid input")
            })
            .expect_err("refused");
            assert_eq!(error.code, "invalid-input");
        }
    }
}
