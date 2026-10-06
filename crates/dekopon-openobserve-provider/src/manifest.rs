//! Closed input schemas derived by the SDK from these four typed capability inputs.
use dekopon_otel_query_core::fit::{
    DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES_CEILING, MIN_OUTPUT_BYTES,
};
use dekopon_otel_query_core::query::{DEFAULT_GROUPS, DEFAULT_LIMIT, MAX_GROUPS, MAX_LIMIT};
use dekopon_otel_query_core::window::MAX_WINDOW_SECONDS;
use serde_json::{Value, json};

fn scope_properties(row: bool) -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    if row {
        properties.insert(
            "sinceSeconds".into(),
            json!({
                "type": "integer", "minimum": 1, "maximum": MAX_WINDOW_SECONDS,
                "description": "How far back to look, in seconds; at most 24 hours."
            }),
        );
    }
    properties.insert("format".into(), json!({
        "type": "string", "enum": ["json", "table"], "default": "json",
        "description": "One JSON object or a fixed-width table; both carry truncation information."
    }));
    properties.insert("maxOutputBytes".into(), json!({
        "type": "integer", "minimum": MIN_OUTPUT_BYTES, "maximum": MAX_OUTPUT_BYTES_CEILING,
        "default": DEFAULT_MAX_OUTPUT_BYTES,
        "description": "Fit the result under this many bytes; the grant's ceiling is authoritative."
    }));
    properties
}
fn schema(properties: serde_json::Map<String, Value>, required: &[&str]) -> Value {
    json!({"type":"object", "required": required, "additionalProperties": false, "properties": properties})
}
/// Projected trace query schema.
pub(crate) fn trace_schema() -> Value {
    let mut properties = scope_properties(true);
    properties.insert(
        "traceId".into(),
        json!({
            "type": "string", "minLength": 32, "maxLength": 32,
            "pattern": "^[0-9a-fA-F]{32}$"
        }),
    );
    properties.insert(
        "limit".into(),
        json!({"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT}),
    );
    schema(properties, &["traceId", "sinceSeconds"])
}
/// Fixed agent aggregate schema.
pub(crate) fn agent_stats_schema() -> Value {
    let mut properties = scope_properties(false);
    properties.insert("agent".into(), json!({
        "type": "string", "minLength": 1, "maxLength": 128,
        "pattern": "^[A-Za-z0-9_-]+$",
        "description": "Cedar cannot compare the named agent against context.agent; a grant can name any agent."
    }));
    schema(properties, &["agent"])
}
/// Broker row and aggregate input schemas.
pub(crate) fn broker_schema(views: &[&str], grouped: bool) -> Value {
    let mut properties = scope_properties(!grouped);
    properties.insert("view".into(), json!({"type":"string", "enum": views}));
    if grouped {
        properties.insert(
            "by".into(),
            json!({
                "type": "string", "enum": ["provider", "capability"], "default": "provider"
            }),
        );
        properties.insert(
            "limit".into(),
            json!({
                "type": "integer", "minimum": 1, "maximum": MAX_GROUPS, "default": DEFAULT_GROUPS
            }),
        );
        schema(properties, &["view"])
    } else {
        schema(properties, &["view", "sinceSeconds"])
    }
}
