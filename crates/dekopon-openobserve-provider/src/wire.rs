//! The OpenObserve `_search` wire: one POST, one JSON envelope, one `hits` array.
//!
//! `POST <base>/api/<org>/_search?type=traces|logs` with
//! `{"query":{"sql":…,"start_time":<µs>,"end_time":<µs>,"from":0,"size":N}}`. That is the shape
//! dekopon's own `examples/otel-traces/smoke-test.sh` has used since the telemetry stack landed, so
//! it is a shape with a working witness rather than one read off a docs page.
//!
//! No `authorization` header is ever set here. The credential is a DRN-bound Basic header injected
//! inside the broker's native HTTP engine, for destinations inside its binding, where no guest can
//! observe it — and the host rejects an `authorization` header from a guest by construction rather
//! than overwriting it.

use dekopon_otel_query_core::query::{QueryError, Scope, Signal};
use dekopon_otel_query_core::window::Window;
use dekopon_provider_http::{Header, Request, Response, method};
use serde_json::Value;

/// The most bytes of an upstream error body this provider will quote back.
const ERROR_EXCERPT_BYTES: usize = 240;

/// Builds one `_search` request.
pub(crate) fn search(
    scope: &Scope,
    signal: Signal,
    sql: &str,
    window: Window,
    size: u32,
) -> Result<Request, QueryError> {
    let uri = format!(
        "{}/api/{}/_search?type={}",
        scope.url,
        scope.org,
        signal.as_str()
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "query": {
            "sql": sql,
            "start_time": window.start_us(),
            "end_time": window.end_us(),
            "from": 0,
            "size": size,
        }
    }))
    .map_err(|error| {
        QueryError::invalid(format!("the request body would not serialize: {error}"))
    })?;
    let request = Request::new(method::POST, uri)
        .map_err(|error| QueryError::invalid(error.to_string()))?
        .with_header(
            Header::text("content-type", "application/json")
                .map_err(|error| QueryError::invalid(error.to_string()))?,
        )
        .with_body(body);
    Ok(request)
}

/// Reads `hits` and `total` out of one answer, or classifies the refusal.
pub(crate) fn hits(response: &Response) -> Result<(Vec<Value>, u64), QueryError> {
    if response.status != 200 {
        return Err(QueryError::upstream(describe_failure(response)));
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|error| {
        QueryError::upstream(format!("the store's answer was not JSON: {error}"))
    })?;
    let rows = match body.get("hits") {
        Some(Value::Array(rows)) => rows.clone(),
        _ => {
            return Err(QueryError::upstream(
                "the store's answer carried no hits array",
            ));
        }
    };
    let total = body
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(rows.len() as u64);
    Ok((rows, total))
}

/// What went wrong upstream, said once, bounded, and without inventing a cause.
///
/// The status is the fact worth carrying — 401 is a credential the owner has to fix, 400 is a
/// statement the caller has to fix — and the store's own `message` is the only useful detail, so it
/// is quoted to a fixed ceiling rather than dropped or passed through whole.
fn describe_failure(response: &Response) -> String {
    let hint = match response.status {
        401 | 403 => " (the injected credential was refused; check the OpenObserve user's role)",
        400 => {
            " (the statement was rejected; folded column names are audit_event, decision_allowed, usage_input_tokens)"
        }
        404 => " (no such organization or stream at that URL)",
        429 => " (the store is rate limiting)",
        _ => "",
    };
    let detail = serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|body| {
            ["message", "error", "error_detail"]
                .into_iter()
                .find_map(|key| body.get(key).and_then(Value::as_str).map(str::to_owned))
        })
        .map(|message| {
            let mut excerpt: String = message.chars().take(ERROR_EXCERPT_BYTES).collect();
            if message.chars().count() > ERROR_EXCERPT_BYTES {
                excerpt.push('…');
            }
            format!(": {excerpt}")
        })
        .unwrap_or_default();
    format!("the store answered {}{hint}{detail}", response.status)
}

#[cfg(test)]
mod tests {
    use dekopon_otel_query_core::query::{Scope, Signal};
    use dekopon_otel_query_core::window::Window;
    use dekopon_provider_http::Response;
    use serde_json::Value;

    use super::{hits, search};

    fn scope() -> Scope {
        Scope {
            url: "http://rpi.lan/openobserve".to_owned(),
            org: "default".to_owned(),
            stream: "dekopon".to_owned(),
            since_seconds: 86_400,
            max_output_bytes: 65_536,
        }
    }

    /// The URL, the method, the headers, and the microsecond arithmetic, asserted exactly.
    #[test]
    fn one_search_is_one_post_with_the_window_in_microseconds() {
        let window = Window::ending_at(1_789_000_000_000_000, 3_600);
        let request = search(&scope(), Signal::Traces, "SELECT 1", window, 100).expect("a request");
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.uri,
            "http://rpi.lan/openobserve/api/default/_search?type=traces"
        );
        let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
        assert_eq!(body["query"]["sql"], "SELECT 1");
        assert_eq!(body["query"]["start_time"], 1_788_996_400_000_000_u64);
        assert_eq!(body["query"]["end_time"], 1_789_000_000_000_000_u64);
        assert_eq!(body["query"]["from"], 0);
        assert_eq!(body["query"]["size"], 100);
    }

    /// The guest never sets `authorization`; the broker injects it where no guest can see it.
    #[test]
    fn the_guest_never_sets_an_authorization_header() {
        let request = search(
            &scope(),
            Signal::Logs,
            "SELECT 1",
            Window::ending_at(1_789_000_000_000_000, 60),
            10,
        )
        .expect("a request");
        let names: Vec<String> = request
            .headers
            .iter()
            .map(|header| header.name.to_ascii_lowercase())
            .collect();
        assert_eq!(names, ["content-type"]);
        assert!(request.uri.ends_with("type=logs"));
    }

    #[test]
    fn an_empty_answer_is_zero_rows_and_not_an_error() {
        let response = Response {
            status: 200,
            headers: Vec::new(),
            body: br#"{"took":3,"hits":[],"total":0}"#.to_vec(),
        };
        let (rows, total) = hits(&response).expect("an answer");
        assert!(rows.is_empty());
        assert_eq!(total, 0);
    }

    /// A refusal says the status, what an owner would change, and the store's own message once.
    #[test]
    fn a_refusal_names_the_status_and_the_thing_to_fix() {
        let unauthorized = Response {
            status: 401,
            headers: Vec::new(),
            body: br#"{"code":401,"message":"Unauthorized Access"}"#.to_vec(),
        };
        let error = hits(&unauthorized).expect_err("refused");
        assert_eq!(error.code(), "upstream-failure");
        assert!(error.message().contains("401"), "{error}");
        assert!(error.message().contains("credential"), "{error}");
        assert!(error.message().contains("Unauthorized Access"), "{error}");

        let bad_sql = Response {
            status: 400,
            headers: Vec::new(),
            body: br#"{"code":400,"message":"sql parser error: Expected an SQL statement"}"#
                .to_vec(),
        };
        let error = hits(&bad_sql).expect_err("refused");
        assert!(error.message().contains("audit_event"), "{error}");
    }

    /// A store that answers with an unbounded body does not get to write the model's context.
    #[test]
    fn an_upstream_message_is_quoted_to_a_ceiling() {
        let body = serde_json::json!({"message": "x".repeat(10_000)});
        let response = Response {
            status: 400,
            headers: Vec::new(),
            body: serde_json::to_vec(&body).expect("serializes"),
        };
        let error = hits(&response).expect_err("refused");
        assert!(error.message().len() < 512, "{}", error.message().len());
        assert!(error.message().ends_with('…'), "{error}");
    }

    #[test]
    fn a_body_that_is_not_a_search_answer_is_an_upstream_failure() {
        for body in [&b"not json"[..], &b"{\"took\":1}"[..]] {
            let response = Response {
                status: 200,
                headers: Vec::new(),
                body: body.to_vec(),
            };
            assert!(hits(&response).is_err());
        }
    }
}
