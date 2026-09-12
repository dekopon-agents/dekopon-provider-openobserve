//! The manifest: what a model is shown, and what the broker checks its constraint sets against.
//!
//! Five capabilities, all `read-only`, over three command words. The split is #250's, and it is a
//! Cedar split rather than a code one: `openobserve.search` reads whatever a statement selects,
//! transcripts included, so it goes to an operator's own agent; `openobserve.agent-stats` and the
//! two `broker` ids return projected numbers and can be granted more widely. One capability id per
//! grant an owner would plausibly want to write separately, and no more.
//!
//! The input schemas are closed (`additionalProperties: false`) and camelCase, because the
//! sandboxed shell rewrites `--kebab-flags` into camelCase JSON keys — a snake_case field with
//! `deny_unknown_fields` is unreachable through the flag form, which is the bug
//! `dekopon-provider-mediawiki` v0.1.0 shipped.

use dekopon_otel_query_core::fit::{
    DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES_CEILING, MIN_OUTPUT_BYTES,
};
use dekopon_otel_query_core::query::{DEFAULT_LIMIT, DEFAULT_ORG, DEFAULT_STREAM, MAX_LIMIT};
use dekopon_otel_query_core::window::MAX_WINDOW_SECONDS;
use dekopon_provider_sdk::{
    EffectKind, ProviderApiVersion, ProviderCapability, ProviderManifest, RiskLevel,
};
use serde_json::{Value, json};

use crate::{AGENT_WORD, BROKER_WORD, PROVIDER_ID, RAW_WORD, capabilities};

/// The component's manifest.
pub(crate) fn manifest() -> ProviderManifest {
    let ids = capabilities();
    ProviderManifest {
        api_version: ProviderApiVersion::V1Alpha1,
        id: PROVIDER_ID.parse().expect("static provider ID"),
        description: "Reads an OpenObserve telemetry store over its _search API: one agent's own \
                      turns and tokens, the broker's fleet view, and bounded raw search"
            .to_owned(),
        command_words: vec![
            RAW_WORD.to_owned(),
            AGENT_WORD.to_owned(),
            BROKER_WORD.to_owned(),
        ],
        capabilities: vec![
            ProviderCapability {
                id: ids.search.clone(),
                description: "Runs one read-only statement against the store and returns its \
                              rows. Returns whatever the statement selects, conversation text \
                              included, so it belongs to an operator's own agent"
                    .to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Medium,
                input_schema: search_schema(),
            },
            ProviderCapability {
                id: ids.trace.clone(),
                description: "Returns one trace's spans as a projected skeleton: ids, operation, \
                              service, kind, status, and timings. Never span events, which is \
                              where conversation text lives"
                    .to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                input_schema: trace_schema(),
            },
            ProviderCapability {
                id: ids.agent_stats.clone(),
                description: "One object of an agent's own numbers over the window: sessions, \
                              model turns, latency percentiles, token totals by kind, capability \
                              calls by id, and denials by reason"
                    .to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                input_schema: agent_stats_schema(),
            },
            ProviderCapability {
                id: ids.broker_providers.clone(),
                description: "What the broker loaded at its last boot inside the window: provider \
                              id, artifact digest prefix, capability and command-word counts, and \
                              compile time. Never a path"
                    .to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                input_schema: broker_schema(&["providers"], false),
            },
            ProviderCapability {
                id: ids.broker_usage.clone(),
                description: "The fleet view over the window: calls grouped by provider, \
                              capability, or agent, or refusals grouped by reason. Every agent's \
                              counts, which is what a fleet view is"
                    .to_owned(),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                input_schema: broker_schema(&["usage", "denials"], true),
            },
        ],
    }
}

/// The fields every capability takes: where the store is, and over what window.
fn scope_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "url".to_owned(),
        json!({
            "type": "string",
            "minLength": 8,
            "maxLength": 512,
            "pattern": "^https?://",
            "description":
                "The store's base URL, with no trailing slash: https://rpi.lan/openobserve. A host \
                 outside the grant's allowedHosts is a denied egress, never a leak."
        }),
    );
    properties.insert(
        "org".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": "^[A-Za-z0-9_-]+$",
            "default": DEFAULT_ORG,
            "description": "The store's organization path segment."
        }),
    );
    properties.insert(
        "stream".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": "^[A-Za-z0-9_-]+$",
            "default": DEFAULT_STREAM,
            "description":
                "The stream both dekopon daemons export into. stream-name=dekopon routes traces \
                 and logs here; spans are not in the default trace stream."
        }),
    );
    properties.insert(
        "sinceSeconds".to_owned(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_WINDOW_SECONDS,
            "description":
                "How far back to look, in seconds. Capped at 30 days, which is the homelab store's \
                 retention; a longer window returns nothing anyway."
        }),
    );
    properties.insert(
        "maxOutputBytes".to_owned(),
        json!({
            "type": "integer",
            "minimum": MIN_OUTPUT_BYTES,
            "maximum": MAX_OUTPUT_BYTES_CEILING,
            "default": DEFAULT_MAX_OUTPUT_BYTES,
            "description":
                "Fit the result under this many bytes, dropping rows from the tail and reporting \
                 truncated with an exact omittedRows. The grant's own maxOutputBytes is the real \
                 ceiling and refuses rather than truncates."
        }),
    );
    properties
}

fn limit_property() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": MAX_LIMIT,
        "default": DEFAULT_LIMIT,
        "description": "Rows to ask the store for."
    })
}

fn schema(properties: serde_json::Map<String, Value>, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "required": required,
        "additionalProperties": false,
        "properties": properties
    })
}

fn search_schema() -> Value {
    let mut properties = scope_properties();
    properties.insert(
        "signal".to_owned(),
        json!({
            "type": "string",
            "enum": ["traces", "logs"],
            "description":
                "Which signal to search. Spans are traces; brokerd's audit records are logs."
        }),
    );
    properties.insert(
        "sql".to_owned(),
        json!({
            "type": "string",
            "minLength": 7,
            "maxLength": 8192,
            "description":
                "One statement, starting with SELECT or a WITH that ends in one; a second \
                 statement is refused. Attribute names are folded to letters, digits, and \
                 underscore: audit.event is audit_event, usage.input_tokens is \
                 usage_input_tokens, decision.allowed is decision_allowed."
        }),
    );
    properties.insert("limit".to_owned(), limit_property());
    schema(properties, &["url", "sinceSeconds", "signal", "sql"])
}

fn trace_schema() -> Value {
    let mut properties = scope_properties();
    properties.insert(
        "traceId".to_owned(),
        json!({
            "type": "string",
            "minLength": 32,
            "maxLength": 32,
            "pattern": "^[0-9a-fA-F]{32}$",
            "description": "The 32-hexadecimal-character W3C trace id."
        }),
    );
    properties.insert("limit".to_owned(), limit_property());
    schema(properties, &["url", "sinceSeconds", "traceId"])
}

fn agent_stats_schema() -> Value {
    let mut properties = scope_properties();
    properties.insert(
        "agent".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "pattern": "^[A-Za-z0-9_-]+$",
            "description":
                "Whose numbers. A flag in 0.1.0: Cedar decides on context.agent, which the broker \
                 stamps, and it cannot compare provider input against context — so a grant of this \
                 capability can name any agent. The dekopon:meta/caller import that would make the \
                 scope a fact of the invocation does not exist yet."
        }),
    );
    schema(properties, &["url", "sinceSeconds", "agent"])
}

fn broker_schema(views: &[&str], grouped: bool) -> Value {
    let mut properties = scope_properties();
    properties.insert(
        "view".to_owned(),
        json!({
            "type": "string",
            "enum": views,
            "description": "Which fleet view."
        }),
    );
    if grouped {
        properties.insert(
            "by".to_owned(),
            json!({
                "type": "string",
                "enum": ["provider", "capability", "agent"],
                "default": "provider",
                "description": "How usage groups. Ignored by denials."
            }),
        );
        properties.insert("limit".to_owned(), limit_property());
    }
    schema(properties, &["url", "sinceSeconds", "view"])
}

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{EffectKind, RiskLevel};

    use super::manifest;
    use crate::{AGENT_WORD, BROKER_WORD, RAW_WORD};

    /// Command words cannot contain a separator: `dekopon_core::command_word_conflicts` refuses a
    /// word that parses as a capability identifier. All three are separator-free, and none is a
    /// reserved shell word.
    #[test]
    fn the_three_command_words_are_separator_free() {
        for word in [RAW_WORD, AGENT_WORD, BROKER_WORD] {
            assert!(!word.contains(['.', '-', '_']), "{word}");
        }
        assert_eq!(manifest().command_words, ["openobserve", "agent", "broker"]);
    }

    /// Every capability is a read. The broker refuses to start when a constraint set disagrees.
    #[test]
    fn every_capability_is_read_only_and_named_after_the_provider() {
        let manifest = manifest();
        assert_eq!(manifest.id.as_str(), "openobserve");
        assert_eq!(manifest.capabilities.len(), 5);
        for capability in &manifest.capabilities {
            assert_eq!(capability.effect, EffectKind::ReadOnly, "{}", capability.id);
            assert!(
                capability.id.as_str().starts_with("openobserve."),
                "{}",
                capability.id
            );
        }
        let ids: Vec<&str> = manifest
            .capabilities
            .iter()
            .map(|capability| capability.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "openobserve.search",
                "openobserve.trace",
                "openobserve.agent-stats",
                "openobserve.broker-providers",
                "openobserve.broker-usage",
            ]
        );
    }

    /// The raw search is the one capability that can return a transcript, and its risk says so.
    #[test]
    fn only_the_raw_search_is_medium_risk() {
        for capability in manifest().capabilities {
            let expected = if capability.id.as_str() == "openobserve.search" {
                RiskLevel::Medium
            } else {
                RiskLevel::Low
            };
            assert_eq!(capability.risk, expected, "{}", capability.id);
        }
    }

    /// Closed schemas, camelCase keys, and the window cap stated where a model reads it.
    #[test]
    fn the_schemas_are_closed_camel_case_and_carry_the_bounds() {
        for capability in manifest().capabilities {
            let schema = &capability.input_schema;
            assert_eq!(schema["additionalProperties"], false, "{}", capability.id);
            let properties = schema["properties"].as_object().expect("properties");
            for key in properties.keys() {
                assert!(!key.contains('_'), "{}: {key} is snake_case", capability.id);
            }
            assert_eq!(
                properties["sinceSeconds"]["maximum"],
                30 * 24 * 60 * 60,
                "{}",
                capability.id
            );
            assert!(properties.contains_key("url"), "{}", capability.id);
            let required = schema["required"].as_array().expect("required");
            assert!(
                required.contains(&serde_json::json!("url")),
                "{}",
                capability.id
            );
            assert!(
                required.contains(&serde_json::json!("sinceSeconds")),
                "{}",
                capability.id
            );
        }
    }

    /// The folding trap is in the schema a model reads, not only in this repository's README.
    #[test]
    fn the_column_folding_trap_is_stated_in_the_search_schema() {
        let manifest = manifest();
        let search = manifest
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == "openobserve.search")
            .expect("the search capability");
        let description = search.input_schema["properties"]["sql"]["description"]
            .as_str()
            .expect("a description");
        assert!(description.contains("audit_event"), "{description}");
        assert!(description.contains("usage_input_tokens"), "{description}");
    }

    /// #248's consequence, in the place a model will actually read it.
    #[test]
    fn the_agent_flag_documents_the_cedar_consequence() {
        let manifest = manifest();
        let stats = manifest
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == "openobserve.agent-stats")
            .expect("the agent-stats capability");
        let description = stats.input_schema["properties"]["agent"]["description"]
            .as_str()
            .expect("a description");
        assert!(description.contains("context.agent"), "{description}");
        assert!(description.contains("can name any agent"), "{description}");
    }
}
