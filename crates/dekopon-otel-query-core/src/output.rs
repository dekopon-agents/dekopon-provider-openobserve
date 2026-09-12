//! The output shapes, which #250 makes a contract rather than an implementation detail.
//!
//! "The `agent`/`broker` output shapes in this issue are the contract every backend must produce
//! byte-for-byte in schema; the core crate owns the serializers." So they are built here, from
//! numbers a backend folded, and a backend that computed them differently still emits the same
//! keys in the same nesting.
//!
//! Every key is camelCase, matching the input wire and matching what the shell's flag rewriting
//! produces, so a `jq '.turns.durationMs.p95'` written against one backend works against the next.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::fit::fit_object;
use crate::window::Window;

/// Latency, as the two percentiles a model actually reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Latency {
    /// The median, in whole milliseconds.
    pub p50: u64,
    /// The 95th percentile, in whole milliseconds.
    pub p95: u64,
}

/// Token totals by kind, summed over the window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tokens {
    /// Prompt tokens billed.
    pub input: u64,
    /// Prompt tokens served from the provider's cache.
    pub cached_input: u64,
    /// Completion tokens.
    pub output: u64,
    /// Reasoning tokens, where the model reports them separately.
    pub reasoning_output: u64,
    /// Whatever the provider called the total.
    pub total: u64,
}

/// Everything `agent stats` folded, before it is fitted and serialized.
#[derive(Clone, Debug, Default)]
pub struct AgentStats {
    /// Model turns seen in the window.
    pub turns: u64,
    /// Turns whose `outcome` was `succeeded`.
    pub succeeded: u64,
    /// Turns whose `outcome` was anything else.
    pub failed: u64,
    /// Turn latency.
    pub latency: Latency,
    /// Token totals.
    pub tokens: Tokens,
    /// Sessions folded, and whether the store had more than the fold took.
    pub sessions: u64,
    /// Capability calls by id.
    pub capabilities: BTreeMap<String, u64>,
    /// Denials by reason.
    pub denials: BTreeMap<String, u64>,
    /// Whether anything was left out of the fold.
    pub truncated: bool,
}

/// Serializes `agent stats` into #250's shape and fits it under `max_output_bytes`.
#[must_use]
pub fn agent_stats(
    agent: &str,
    window: Window,
    stats: &AgentStats,
    max_output_bytes: usize,
) -> Value {
    let mut object = Map::new();
    object.insert("agent".to_owned(), json!(agent));
    object.insert("window".to_owned(), window.to_json());
    object.insert("sessions".to_owned(), json!(stats.sessions));
    object.insert(
        "turns".to_owned(),
        json!({
            "count": stats.turns,
            "succeeded": stats.succeeded,
            "failed": stats.failed,
            "durationMs": {"p50": stats.latency.p50, "p95": stats.latency.p95},
        }),
    );
    object.insert(
        "tokens".to_owned(),
        json!({
            "input": stats.tokens.input,
            "cachedInput": stats.tokens.cached_input,
            "output": stats.tokens.output,
            "reasoningOutput": stats.tokens.reasoning_output,
            "total": stats.tokens.total,
        }),
    );
    object.insert("capabilities".to_owned(), counts(&stats.capabilities));
    object.insert("denials".to_owned(), counts(&stats.denials));
    object.insert("maxOutputBytes".to_owned(), json!(max_output_bytes));
    let mut fitted = fit_object(object, "capabilities");
    if stats.truncated && fitted["truncated"] == Value::Bool(false) {
        fitted["truncated"] = Value::Bool(true);
    }
    fitted
}

/// Serializes `broker providers` into #249's shape.
#[must_use]
pub fn broker_providers(
    booted_at_us: Option<u64>,
    providers: Vec<Value>,
    truncated: bool,
) -> Value {
    json!({
        "bootedAtUs": booted_at_us,
        "providers": providers,
        "truncated": truncated,
    })
}

/// A `{name: count}` object with a stable key order.
#[must_use]
pub fn counts(entries: &BTreeMap<String, u64>) -> Value {
    let mut object = Map::new();
    for (key, value) in entries {
        object.insert(key.clone(), json!(value));
    }
    Value::Object(object)
}

/// The nearest-rank percentile of a sorted slice, which is what a fold over rows can honestly
/// claim.
///
/// Nearest-rank rather than interpolated: the inputs are whole milliseconds out of the store, an
/// interpolated p95 of whole milliseconds is a number no record contains, and #250's shape says
/// `durationMs`. Returns 0 for an empty slice, because "no turns" has no latency and inventing one
/// would be worse than a zero a reader can see next to `"count": 0`.
#[must_use]
pub fn percentile(sorted: &[u64], quantile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (quantile * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AgentStats, Latency, Tokens, agent_stats, broker_providers, percentile};
    use crate::window::Window;

    fn window() -> Window {
        Window::ending_at(1_789_000_000_000_000, 86_400)
    }

    /// The shape is the contract, so it is asserted key by key against #250.
    #[test]
    fn agent_stats_matches_the_documented_shape() {
        let mut capabilities = BTreeMap::new();
        capabilities.insert("gh.pull-request.read".to_owned(), 17);
        capabilities.insert("gh.pull-request.files".to_owned(), 12);
        let mut denials = BTreeMap::new();
        denials.insert("policy-denied".to_owned(), 1);

        let value = agent_stats(
            "reviewer",
            window(),
            &AgentStats {
                turns: 41,
                succeeded: 39,
                failed: 2,
                latency: Latency {
                    p50: 2_140,
                    p95: 9_800,
                },
                tokens: Tokens {
                    input: 183_220,
                    cached_input: 121_004,
                    output: 22_410,
                    reasoning_output: 0,
                    total: 205_630,
                },
                sessions: 6,
                capabilities,
                denials,
                truncated: false,
            },
            64 * 1024,
        );

        assert_eq!(value["agent"], "reviewer");
        assert_eq!(value["window"]["sinceUs"], 1_788_913_600_000_000_u64);
        assert_eq!(value["window"]["untilUs"], 1_789_000_000_000_000_u64);
        assert_eq!(value["turns"]["count"], 41);
        assert_eq!(value["turns"]["succeeded"], 39);
        assert_eq!(value["turns"]["failed"], 2);
        assert_eq!(value["turns"]["durationMs"]["p50"], 2_140);
        assert_eq!(value["turns"]["durationMs"]["p95"], 9_800);
        assert_eq!(value["tokens"]["input"], 183_220);
        assert_eq!(value["tokens"]["cachedInput"], 121_004);
        assert_eq!(value["tokens"]["reasoningOutput"], 0);
        assert_eq!(value["tokens"]["total"], 205_630);
        assert_eq!(value["capabilities"]["gh.pull-request.read"], 17);
        assert_eq!(value["denials"]["policy-denied"], 1);
        assert_eq!(value["truncated"], false);
        assert!(value.get("maxOutputBytes").is_none());
        for key in value.as_object().expect("an object").keys() {
            assert!(!key.contains('_'), "{key} is snake_case");
        }
    }

    /// A fold that dropped sessions says so even when the serialized object fit comfortably.
    #[test]
    fn a_truncated_fold_is_reported_even_when_the_object_fits() {
        let value = agent_stats(
            "reviewer",
            window(),
            &AgentStats {
                sessions: 200,
                truncated: true,
                ..AgentStats::default()
            },
            64 * 1024,
        );
        assert_eq!(value["truncated"], true);
        assert_eq!(value["sessions"], 200);
    }

    #[test]
    fn nearest_rank_percentiles_are_the_numbers_the_store_actually_holds() {
        let sorted = [10_u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&sorted, 0.5), 50);
        assert_eq!(percentile(&sorted, 0.95), 100);
        assert_eq!(percentile(&[7], 0.5), 7);
        assert_eq!(percentile(&[7], 0.95), 7);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn broker_providers_reports_an_absent_boot_as_null() {
        let value = broker_providers(None, Vec::new(), false);
        assert!(value["bootedAtUs"].is_null());
        assert_eq!(value["providers"].as_array().expect("an array").len(), 0);
        assert_eq!(value["truncated"], false);
    }
}
