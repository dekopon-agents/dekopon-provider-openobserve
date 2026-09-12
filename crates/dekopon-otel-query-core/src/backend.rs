//! The seam between the backend-neutral words and one store's wire protocol.
//!
//! #250 specifies `fn plan(Word, Window) -> Vec<HttpRequest>` and `fn fold(Word, Vec<HttpResponse>)
//! -> Output`. [`Backend::plan`] here takes the answers so far as well, and returns at most one
//! request. The reason is a fact about dekopon's own telemetry rather than a preference: an agent's
//! identity is on the `gateway.session` span and its token counts are on the `prompt.model_turn`
//! spans beneath it, so the second statement's `trace_id IN (…)` is not knowable until the first
//! answer is in hand. A flat `Vec<HttpRequest>` cannot express that, and a subquery would bet the
//! word on which DataFusion features the deployed store exposes.
//!
//! Each step is one authorized host call, counted against the constraint set's `maxRequests` by the
//! broker. [`run`] is the driver; it is generic over the transport so every backend is testable as
//! ordinary Rust against recorded bytes.

use dekopon_provider_http::{HttpError, Request, Response};
use serde_json::Value;

use crate::query::{Query, QueryError};

/// The most host calls any one word may make.
///
/// `agent stats` is the widest at three — sessions, then turns, then decisions — and the ceiling
/// exists so a backend bug cannot turn one invocation into an unbounded call loop. The broker's
/// `maxRequests` is the real bound; this is the guest refusing to try.
pub const MAX_STEPS: usize = 4;

/// One telemetry store's wire protocol.
pub trait Backend {
    /// The next request for this query, given the answers so far, or `None` when there is nothing
    /// left to ask.
    fn plan(&self, query: &Query, prior: &[Response]) -> Result<Option<Request>, QueryError>;

    /// The answer, from every response the plan collected.
    fn fold(&self, query: &Query, responses: &[Response]) -> Result<Value, QueryError>;
}

/// Drives one query to an answer over an injected transport.
///
/// Taking `send` as a closure is what lets every test assert the exact bytes of every request and
/// the exact projection of every response with no network and no host: the seam is the one the
/// component uses, so what the tests exercise is what ships.
pub fn run<B, F>(backend: &B, query: &Query, mut send: F) -> Result<Value, QueryError>
where
    B: Backend,
    F: FnMut(Request) -> Result<Response, HttpError>,
{
    let mut responses = Vec::new();
    for _ in 0..MAX_STEPS {
        let Some(request) = backend.plan(query, &responses)? else {
            return backend.fold(query, &responses);
        };
        let response = send(request).map_err(transport)?;
        responses.push(response);
    }
    Err(QueryError::upstream(format!(
        "the plan did not finish within {MAX_STEPS} requests"
    )))
}

/// Classifies a transport failure without repeating whatever the host put in the message.
///
/// The host's message can name a resolved address; the guest's error travels to a model. So the
/// code is kept and the text is this provider's own.
fn transport(error: HttpError) -> QueryError {
    use dekopon_provider_http::HttpErrorCode as Code;
    let reason = match error.code {
        Code::Denied => {
            "the broker denied the request; the store's host is outside allowedHosts, \
             or the method or path is outside the credential binding"
        }
        Code::HostCallLimit => {
            "the invocation's request budget is spent; raise maxRequests or \
             narrow the window"
        }
        Code::Dns => "the store's host did not resolve",
        Code::Connect => "the store refused the connection",
        Code::Tls => "the TLS handshake with the store failed",
        Code::Timeout => "the store did not answer within the constraint set's timeoutMs",
        Code::ResponseTooLarge => {
            "the answer exceeded maxResponseBytes; lower --limit or narrow \
             the projection"
        }
        Code::RequestTooLarge => "the statement exceeded maxRequestBytes",
        Code::InvalidUri => "the store URL was rejected as malformed",
        Code::InvalidMethod | Code::InvalidHeader => "the request was rejected as malformed",
        Code::Protocol => "the store spoke something other than HTTP",
        Code::Internal => "the broker's HTTP engine failed",
    };
    QueryError::upstream(reason)
}

#[cfg(test)]
mod tests {
    use dekopon_provider_http::{HttpError, HttpErrorCode, Request, Response};
    use serde_json::{Value, json};

    use super::{Backend, MAX_STEPS, run};
    use crate::query::{Query, QueryError, Scope, SearchQuery, Signal};

    struct Endless;

    impl Backend for Endless {
        fn plan(&self, _query: &Query, _prior: &[Response]) -> Result<Option<Request>, QueryError> {
            Ok(Some(
                Request::new("POST", "http://rpi.lan/openobserve").expect("a request"),
            ))
        }

        fn fold(&self, _query: &Query, _responses: &[Response]) -> Result<Value, QueryError> {
            Ok(json!({}))
        }
    }

    struct Once;

    impl Backend for Once {
        fn plan(&self, _query: &Query, prior: &[Response]) -> Result<Option<Request>, QueryError> {
            if prior.is_empty() {
                Ok(Some(
                    Request::new("POST", "http://rpi.lan/openobserve").expect("a request"),
                ))
            } else {
                Ok(None)
            }
        }

        fn fold(&self, _query: &Query, responses: &[Response]) -> Result<Value, QueryError> {
            Ok(json!({"responses": responses.len()}))
        }
    }

    fn query() -> Query {
        Query::Search(SearchQuery {
            scope: Scope {
                url: "http://rpi.lan/openobserve".to_owned(),
                org: "default".to_owned(),
                stream: "dekopon".to_owned(),
                since_seconds: 3_600,
                max_output_bytes: 65_536,
                format: crate::query::Format::Json,
            },
            signal: Signal::Traces,
            sql: "SELECT 1".to_owned(),
            limit: 1,
        })
    }

    fn empty() -> Response {
        Response {
            status: 200,
            headers: Vec::new(),
            body: b"{}".to_vec(),
        }
    }

    #[test]
    fn the_driver_stops_when_the_plan_stops() {
        let mut calls = 0_usize;
        let output = run(&Once, &query(), |_request| {
            calls += 1;
            Ok(empty())
        })
        .expect("an answer");
        assert_eq!(calls, 1);
        assert_eq!(output["responses"], 1);
    }

    /// A backend that never finishes is the guest's bug, and it costs a bounded number of calls.
    #[test]
    fn a_plan_that_never_finishes_is_refused_after_the_ceiling() {
        let mut calls = 0_usize;
        let error = run(&Endless, &query(), |_request| {
            calls += 1;
            Ok(empty())
        })
        .expect_err("the ceiling holds");
        assert_eq!(calls, MAX_STEPS);
        assert_eq!(error.code(), "upstream-failure");
    }

    /// The host's message can name a resolved address; what reaches the model is this crate's text.
    #[test]
    fn a_transport_failure_is_classified_without_echoing_the_host() {
        let error = run(&Once, &query(), |_request| {
            Err(HttpError {
                code: HttpErrorCode::Connect,
                message: "connect 192.168.1.100:80 refused".to_owned(),
            })
        })
        .expect_err("unreachable");
        assert_eq!(error.code(), "upstream-failure");
        assert!(!error.message().contains("192.168.1.100"), "{error}");
        assert!(
            error.message().contains("refused the connection"),
            "{error}"
        );
    }

    /// Every error code maps to text; a new one in the SDK is a compile error, not a fallthrough.
    #[test]
    fn the_denial_and_budget_codes_say_what_an_owner_would_change() {
        for (code, needle) in [
            (HttpErrorCode::Denied, "allowedHosts"),
            (HttpErrorCode::HostCallLimit, "maxRequests"),
            (HttpErrorCode::ResponseTooLarge, "maxResponseBytes"),
            (HttpErrorCode::Timeout, "timeoutMs"),
        ] {
            let error = run(&Once, &query(), |_request| {
                Err(HttpError {
                    code,
                    message: String::new(),
                })
            })
            .expect_err("refused");
            assert!(error.message().contains(needle), "{code:?}: {error}");
        }
    }
}
