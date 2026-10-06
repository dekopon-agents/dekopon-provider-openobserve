//! Pure command grammar: proposals grant no imports and carry no resolved clock window.
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::fit::DEFAULT_MAX_OUTPUT_BYTES;
use crate::query::{BrokerView, Format, UsageGrouping};
use crate::window::parse_since;

/// The three disjoint command actions (the broker does not pass the command word).
#[derive(Debug, Parser)]
#[command(
    name = "openobserve",
    version,
    about = "Bounded OpenObserve telemetry reads",
    long_about = "Three words share this action grammar: openobserve trace --since <DURATION> <TRACE-ID>; agent stats --agent <ID>; broker providers --since <DURATION>; broker usage [--by provider|capability]; broker denials. Only trace and providers take --since (up to 24h). Stats, usage and denials use a fixed five-minute window. Per-actor grouping is not available."
)]
pub struct Commands {
    /// The requested action.
    #[command(subcommand)]
    pub action: Action,
}

/// Supported read actions.
#[derive(Debug, Subcommand)]
pub enum Action {
    /// Project one trace's spans without events or conversation text.
    Trace {
        /// 32-hexadecimal-character trace identifier.
        trace_id: String,
        /// Look back at most 24h (s, m, h, d).
        #[arg(long)]
        since: String,
        /// At most 500 spans.
        #[arg(long, default_value_t = 100, value_parser = dekopon_provider_sdk::clap::value_parser!(u32).range(1..=500))]
        limit: u32,
        /// json or table.
        #[arg(long, default_value = "json", value_parser = ["json", "table"])]
        format: String,
        /// Output fitting ceiling.
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: usize,
    },
    /// One agent's fixed five-minute statistics.
    Stats {
        /// Agent identifier.
        #[arg(long)]
        agent: String,
        /// json or table.
        #[arg(long, default_value = "json", value_parser = ["json", "table"])]
        format: String,
        /// Output fitting ceiling.
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: usize,
    },
    /// Loaded providers since the last boot (row read, at most 24h).
    Providers {
        /// Look back at most 24h.
        #[arg(long)]
        since: String,
        /// json or table.
        #[arg(long, default_value = "json", value_parser = ["json", "table"])]
        format: String,
        /// Output fitting ceiling.
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: usize,
    },
    /// Calls grouped by provider or capability over a fixed five-minute window.
    Usage {
        /// Grouping key; per-agent grouping is not available.
        #[arg(long, default_value = "provider", value_parser = ["provider", "capability"])]
        by: String,
        /// At most 50 groups.
        #[arg(long, default_value_t = 20, value_parser = dekopon_provider_sdk::clap::value_parser!(u32).range(1..=50))]
        limit: u32,
        /// json or table.
        #[arg(long, default_value = "json", value_parser = ["json", "table"])]
        format: String,
        /// Output fitting ceiling.
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: usize,
    },
    /// Denials grouped by capability and reason over a fixed five-minute window.
    Denials {
        /// At most 50 groups.
        #[arg(long, default_value_t = 20, value_parser = dekopon_provider_sdk::clap::value_parser!(u32).range(1..=50))]
        limit: u32,
        /// json or table.
        #[arg(long, default_value = "json", value_parser = ["json", "table"])]
        format: String,
        /// Output fitting ceiling.
        #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES)]
        max_output_bytes: usize,
    },
}

/// Converts the parsed action to a capability suffix and closed JSON input.
///
/// # Errors
/// Refuses invalid duration or format before any authorization or request.
pub fn proposal(action: Action) -> Result<(&'static str, Value), String> {
    let (name, mut input, since, format, max_output_bytes) = match action {
        Action::Trace {
            trace_id,
            since,
            limit,
            format,
            max_output_bytes,
        } => (
            "trace",
            json!({"traceId": trace_id, "limit": limit}),
            Some(since),
            format,
            max_output_bytes,
        ),
        Action::Stats {
            agent,
            format,
            max_output_bytes,
        } => (
            "agent-stats",
            json!({"agent": agent}),
            None,
            format,
            max_output_bytes,
        ),
        Action::Providers {
            since,
            format,
            max_output_bytes,
        } => (
            "broker-providers",
            json!({"view": BrokerView::Providers}),
            Some(since),
            format,
            max_output_bytes,
        ),
        Action::Usage {
            by,
            limit,
            format,
            max_output_bytes,
        } => (
            "broker-usage",
            json!({"view": BrokerView::Usage, "by": if by == "capability" { UsageGrouping::Capability } else { UsageGrouping::Provider }, "limit": limit}),
            None,
            format,
            max_output_bytes,
        ),
        Action::Denials {
            limit,
            format,
            max_output_bytes,
        } => (
            "broker-usage",
            json!({"view": BrokerView::Denials, "limit": limit}),
            None,
            format,
            max_output_bytes,
        ),
    };
    if let Some(since) = since {
        input["sinceSeconds"] = json!(parse_since(&since).map_err(|error| error.to_string())?);
    }
    input["format"] = json!(if format == "table" {
        Format::Table
    } else {
        Format::Json
    });
    input["maxOutputBytes"] = json!(max_output_bytes);
    Ok((name, input))
}
