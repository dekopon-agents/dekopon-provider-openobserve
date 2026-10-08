use dekopon_openobserve_provider::OpenObserveProvider;
use dekopon_provider_sdk::provider::{Header, Response};
use dekopon_provider_sdk_testkit::{Harness, HttpScript, Native};
use serde_json::{Value, json};

fn input() -> Value {
    json!({"traceId":"00000000000000000000000000000001","sinceSeconds":300})
}

fn settings(base: &str) -> Value {
    json!({"baseUrl":base,"org":"default","stream":"synthetic"})
}

fn cassette() -> Value {
    serde_json::from_str(include_str!("cassettes/search-empty.json")).unwrap()
}

fn response() -> Response {
    let fixture = cassette();
    Response {
        status: fixture["response"]["status"]
            .as_u64()
            .unwrap()
            .try_into()
            .unwrap(),
        headers: vec![Header::text("content-type", "application/json").unwrap()],
        body: serde_json::to_vec(&fixture["response"]["body"]["json"]).unwrap(),
    }
}

#[test]
fn synthetic_cassette_replays_root_and_prefixed_bases() {
    let fixture = cassette();
    assert_eq!(fixture["version"], 1);
    for base in [
        "https://fixture.example.test",
        "https://fixture.example.test/openobserve/",
        "https://fixture.example.test/proxy%20path/openobserve",
    ] {
        let native = Native::<OpenObserveProvider>::new()
            .clock(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_789_084_800))
            .settings(settings(base))
            .http(HttpScript::new("fixture.example.test", "POST", response()));
        let output = native.call("openobserve.trace", &input().to_string());
        assert_eq!(output.status, 0, "{}", output.stderr);
        let requests = native.requests();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert_eq!(sent.method, fixture["request"]["method"]);
        assert_eq!(
            sent.uri,
            format!(
                "{}{}?{}",
                base.trim_end_matches('/'),
                fixture["request"]["path"].as_str().unwrap(),
                fixture["request"]["query"].as_str().unwrap()
            )
        );
        let accept = sent
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("accept"))
            .unwrap();
        assert_eq!(
            accept.value,
            fixture["request"]["headers"]["accept"]
                .as_str()
                .unwrap()
                .as_bytes()
        );
        assert!(
            !sent
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("authorization"))
        );
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body, fixture["request"]["body"]["json"]);
        assert!(
            body["query"]["sql"]
                .as_str()
                .unwrap()
                .contains("FROM \"synthetic\"")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({"rows":[],"returned":0,"total":0,"truncated":false,"omittedRows":0})
        );
    }
}

#[test]
fn invalid_and_missing_settings_fail_before_http() {
    let mut invalid = vec![
        Value::Null,
        json!({}),
        json!({"org":"default","stream":"synthetic"}),
        json!({"baseUrl":"https://fixture.example.test","stream":"synthetic"}),
        json!({"baseUrl":"https://fixture.example.test","org":"default"}),
    ];
    for base in [
        json!("https://fixture.example.test?query=1"),
        json!("https://user@fixture.example.test"),
        json!("https://fixture.example.test#fragment"),
        json!("ftp://fixture.example.test"),
        json!("fixture.example.test"),
        json!(42),
    ] {
        let mut value = settings("https://fixture.example.test");
        value["baseUrl"] = base;
        invalid.push(value);
    }
    for key in ["unknown", "url"] {
        let mut value = settings("https://fixture.example.test");
        value[key] = json!("https://fixture.example.test");
        invalid.push(value);
    }
    for value in invalid {
        let native = Native::<OpenObserveProvider>::new().settings(value.clone());
        let output = native.call("openobserve.trace", &input().to_string());
        assert_ne!(output.status, 0, "{value}");
        assert!(
            output.stderr.contains("invalid-settings"),
            "{}",
            output.stderr
        );
        assert!(native.requests().is_empty());
    }
    let native = Native::<OpenObserveProvider>::new();
    let output = native.call("openobserve.trace", &input().to_string());
    assert!(output.stderr.contains("invalid-settings"));
    assert!(native.requests().is_empty());
}

#[test]
fn model_origin_controls_fail_before_http() {
    for key in ["url", "baseUrl", "endpoint", "org", "stream"] {
        let native =
            Native::<OpenObserveProvider>::new().settings(settings("https://fixture.example.test"));
        let mut value = input();
        value[key] = json!("untrusted");
        let output = native.call("openobserve.trace", &value.to_string());
        assert_ne!(output.status, 0, "{key}");
        assert!(native.requests().is_empty());
    }
}

#[test]
fn real_component_replays_synthetic_response() {
    let component =
        std::env::var_os("DEKOPON_PROVIDER_COMPONENT").expect("fresh component required");
    let run = Harness::<OpenObserveProvider>::get(component).http(HttpScript::new(
        "localhost",
        "POST",
        response(),
    ));
    let base = format!("{}/openobserve", run.origin().unwrap());
    let output = run
        .settings(settings(&base))
        .call("openobserve.trace", input())
        .unwrap();
    assert_eq!(output.status, 0, "{}", output.stderr);
    assert_eq!(output.http_calls.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"rows":[],"returned":0,"total":0,"truncated":false,"omittedRows":0})
    );
}
