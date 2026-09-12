//! What the `agent` and `broker` words may name in a `SELECT`, and what nothing may.
//!
//! The rule is positive: those words select an explicit column list and never `SELECT *`. This
//! module is the negative half — the list that must never appear in one — so a projection that
//! grows a column is a test failure rather than a leak. It is `inspect_agent_config`'s `omitted`
//! list (`crates/dekopon-agent/src/meta.rs`) applied to a stats view: credential values and names,
//! raw policy identifiers and digests, principal/subject/channel/transport identifiers, endpoints
//! and paths, and every carrier of conversation text.
//!
//! `openobserve sql` and `openobserve search` are deliberately *not* filtered by this list. They
//! return whatever the statement selects, transcripts included, which is why both are Medium risk
//! and why #250 grants them to an operator's own agent alone. Projecting them would be a bound a
//! determined caller walks around with `SELECT *`, and a bound that can be walked around is worse
//! than an honest capability boundary.

/// Column names the `agent` and `broker` projections must never contain.
///
/// Folded to OpenObserve's column spelling: the store replaces every character outside letters,
/// digits, and underscore, so `decision.allowed` is stored as `decision_allowed`.
pub const FORBIDDEN_COLUMNS: &[&str] = &[
    // Conversation, prompt, answer, script, and tool-output text.
    "answer",
    "prompt",
    "transcript",
    "tool_calls",
    "script",
    "output",
    "body",
    "message",
    // Identity beyond the agent id, which is the scoping key and is shown to the same session.
    "principal",
    "subject",
    "channel",
    "conversation",
    "transport",
    "via",
    // Policy material.
    "policy_ids",
    "policy_digest",
    "policy_revision",
    "decision_digest",
    // Credential names, symbolic or otherwise.
    "credential",
    "secret",
    "secret_sink",
    // Endpoints and paths.
    "path",
    "url_full",
    "server_address",
    "http_calls",
];

/// Column-name prefixes with the same rule, for families rather than single names.
///
/// `gen_ai_` is the whole OpenTelemetry GenAI convention, which is where a model's input and output
/// messages land — the single most sensitive family in the store, and the one a projection would
/// most plausibly acquire by accident while reaching for a token count.
pub const FORBIDDEN_PREFIXES: &[&str] = &["gen_ai_", "transcript_", "chat_", "credential_"];

/// Whether one column name is refused in an `agent` or `broker` projection.
#[must_use]
pub fn is_forbidden(column: &str) -> bool {
    let column = column.trim().to_ascii_lowercase();
    FORBIDDEN_COLUMNS.contains(&column.as_str())
        || FORBIDDEN_PREFIXES
            .iter()
            .any(|prefix| column.starts_with(prefix))
}

/// Panics if a projection this crate assembled names a forbidden column.
///
/// Called from the tests that cover every planned statement. It is a `debug_assert`-shaped check
/// deliberately written as a plain function so the test suite, not the release build, is what
/// enforces it: the component must not carry a panic path for an invariant its own tests prove.
#[must_use]
pub fn first_forbidden<'a>(columns: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    columns.into_iter().find(|column| is_forbidden(column))
}

#[cfg(test)]
mod tests {
    use super::{first_forbidden, is_forbidden};

    #[test]
    fn the_families_that_carry_text_and_identity_are_refused() {
        for forbidden in [
            "answer",
            "Answer",
            "  prompt  ",
            "gen_ai_input_messages",
            "gen_ai_output_messages",
            "policy_digest",
            "credential",
            "secret_sink",
            "path",
            "subject",
            "http_calls",
        ] {
            assert!(is_forbidden(forbidden), "{forbidden} was allowed");
        }
    }

    /// The columns the stats views are built from must stay allowed, or the check is not a check.
    #[test]
    fn the_columns_the_stats_views_need_stay_allowed() {
        for allowed in [
            "trace_id",
            "span_id",
            "operation_name",
            "service_name",
            "span_status",
            "duration",
            "agent",
            "actor_id",
            "actor_kind",
            "capability_id",
            "provider",
            "decision_allowed",
            "decision_reason",
            "outcome",
            "duration_ms",
            "usage_input_tokens",
            "usage_cached_input_tokens",
            "usage_output_tokens",
            "usage_reasoning_output_tokens",
            "usage_total_tokens",
            "model_turn",
            "conversation_turns",
            "artifact_sha256",
            "capabilities",
            "command_words",
            "command_export",
            "compile_ms",
        ] {
            assert!(!is_forbidden(allowed), "{allowed} was refused");
        }
    }

    #[test]
    fn the_first_offender_is_named() {
        assert_eq!(
            first_forbidden(["trace_id", "duration", "credential", "subject"]),
            Some("credential")
        );
        assert_eq!(first_forbidden(["trace_id", "duration"]), None);
    }
}
