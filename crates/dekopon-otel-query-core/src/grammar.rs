//! The three command words, as small command-line programs rendered by the guest.
//!
//! `openobserve --help`, `agent stats --help`, and every usage error are answered here and
//! authorize nothing: the SDK's clap layer renders them as text with an exit status, the way the
//! upstream tool's `main` would. A well-formed argv becomes a *proposal*, which then travels the
//! identical authorization path a direct `cap openobserve.agent-stats {…}` call takes —
//! constraint-set lookup, Cedar, then credential injection inside the broker's HTTP engine. Naming
//! a capability the caller was not granted is a denial, not an escalation.
//!
//! The grammar lives in the backend-neutral crate because #250 put it there: `agent` and `broker`
//! are the same words against any store that speaks dekopon's record schema, and the trace word
//! takes the backend's name. Capability ids arrive as [`Capabilities`].

use dekopon_provider_sdk::clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use dekopon_provider_sdk::{CapabilityId, CommandInvocation, CommandRun, ProviderError, cli};
use serde_json::{Value, json};

use crate::fit::{DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES_CEILING, MIN_OUTPUT_BYTES};
use crate::query::{BrokerView, DEFAULT_LIMIT, MAX_LIMIT, UsageGrouping};
use crate::window::{MAX_WINDOW_SECONDS, parse_since};

/// The capability ids one backend's words propose.
///
/// Ids follow the provider — `openobserve.agent-stats`, `quickwit.agent-stats` — so an owner can
/// grant the fleet view over dekopon's own store without granting it over someone else's, and the
/// audit record names which store was read.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// `<backend> trace`.
    pub trace: CapabilityId,
    /// `agent stats`.
    pub agent_stats: CapabilityId,
    /// `broker providers`.
    pub broker_providers: CapabilityId,
    /// `broker usage` and `broker denials`.
    pub broker_usage: CapabilityId,
}

impl Capabilities {
    /// The four bounded ids for one provider id.
    ///
    /// # Panics
    ///
    /// Panics if `provider` is not a valid provider identifier, which makes a typo a test failure
    /// in the backend that declares it rather than a load-time refusal in someone's broker.
    #[must_use]
    pub fn for_provider(provider: &str) -> Self {
        let id = |suffix: &str| -> CapabilityId {
            format!("{provider}.{suffix}")
                .parse()
                .expect("a valid provider id yields valid capability ids")
        };
        Self {
            trace: id("trace"),
            agent_stats: id("agent-stats"),
            broker_providers: id("broker-providers"),
            broker_usage: id("broker-usage"),
        }
    }

    /// Every id, in manifest order.
    #[must_use]
    pub fn all(&self) -> [&CapabilityId; 4] {
        [
            &self.trace,
            &self.agent_stats,
            &self.broker_providers,
            &self.broker_usage,
        ]
    }
}

/// The `agent` command word, backend-neutral.
pub const AGENT_WORD: &str = "agent";
/// The `broker` command word, backend-neutral.
pub const BROKER_WORD: &str = "broker";

/// Which of a provider's command words an argv belongs to.
///
/// **The guest is never told which word was typed.** `dekopon:provider@0.3.0` declares
/// `run-command: func(argv: list<string>, stdin: option<string>)`, and the broker host resolves the
/// provider *by* the word and then hands the guest the remaining argv alone
/// (`dekopon-broker-host/src/lib.rs`, `run_command(word, argv, stdin)`). A provider with one word
/// never notices; a provider with three has to recover the word from the argv.
///
/// It recovers cleanly because the three vocabularies are disjoint by construction: `trace`
/// belongs to the backend word, `stats` to `agent`, and `providers`, `usage` and `denials`
/// to `broker`. What cannot be recovered is a bare `--help` or an empty argv, which are identical
/// for all three — so those render one overview naming every word and every action, which is more
/// useful than one word's page chosen by a coin flip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    /// `trace`.
    Raw,
    /// `stats`.
    Agent,
    /// `providers`, `usage`, `denials`.
    Broker,
    /// A bare `--help`, `--version`, or an empty argv: ambiguous across all three words.
    Overview,
}

/// Routes one argv to the word that owns it.
#[must_use]
pub fn route(argv: &[String]) -> Route {
    match argv.first().map(String::as_str) {
        Some("trace") => Route::Raw,
        Some("stats") => Route::Agent,
        Some("providers" | "usage" | "denials") => Route::Broker,
        _ => Route::Overview,
    }
}

/// The page a bare `--help` renders: every word, every action, and why it is one page.
#[must_use]
pub fn overview(raw_word: &str, about: &str) -> String {
    format!(
        "{about}\n\n\
         This provider contributes three command words. The component is handed one argv with no \n\
         word attached, so `--help` on its own cannot tell them apart and renders this page; name \n\
         an action to reach that word's own help.\n\n\
         \x20 {raw_word} trace --since <DURATION> <TRACE-ID>\n\
         \x20 agent stats --since <DURATION> --agent <ID>\n\
         \x20 broker providers --since <DURATION>\n\
         \x20 broker usage --since <DURATION> [--by provider|capability|agent]\n\
         \x20 broker denials --since <DURATION>\n\n\
         Every word takes --format json|table. json is the default and is one object with a rows\n\
         array, so `<word> … | jq '.rows[] | .duration_ms'` works with no wrapper; row keys are the\n\
         store's own column names. table renders a fixed-width text table instead.\n\n\
         --since is capped at 30d and --limit at 500. Attribute names are folded to letters, digits\n\
         and underscore: audit.event is the column audit_event.\n\n\
         Try `{raw_word} trace --help`, `agent stats --help`, or `broker usage --help`.\n"
    )
}

const SINCE_HELP: &str = "How far back to look: 30m, 24h, 7d (units s, m, h, d). Capped at 30d, \
                          which is the homelab store's retention";

fn scope_arguments(command: Command) -> Command {
    command
        .arg(
            Arg::new("since")
                .long("since")
                .value_name("DURATION")
                .required(true)
                .help(SINCE_HELP),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .value_parser(["json", "table"])
                .default_value("json")
                .help(
                    "json (default) is one object with a rows array, which pipes straight into \
                     `| jq '.rows[] | .duration_ms'`; table is a fixed-width text table to read \
                     directly. Both carry the truncation marker",
                ),
        )
        .arg(
            Arg::new("max-output-bytes")
                .long("max-output-bytes")
                .value_name("BYTES")
                .value_parser(value_parser!(u64))
                .help(
                    "Fit the result under this many bytes, dropping rows from the tail and \
                     reporting omittedRows. Defaults to 65536; the grant's own maxOutputBytes is \
                     the real ceiling",
                ),
        )
}

fn limit_argument(command: Command) -> Command {
    command.arg(
        Arg::new("limit")
            .long("limit")
            .value_name("N")
            .value_parser(value_parser!(u32).range(1..=i64::from(MAX_LIMIT)))
            .default_value(Box::leak(DEFAULT_LIMIT.to_string().into_boxed_str()) as &'static str)
            .help("Rows to ask the store for, at most 500"),
    )
}

/// Builds the raw word's clap tree under the backend's own name.
#[must_use]
pub fn raw_command(word: &'static str, about: &'static str) -> Command {
    let mut trace = scope_arguments(Command::new("trace").about(
        "Return one trace's spans, projected: no span events, no conversation text, no paths",
    ).arg(
        Arg::new("trace-id")
            .value_name("TRACE-ID")
            .required(true)
            .help("The 32-hexadecimal-character W3C trace id"),
    ));
    trace = limit_argument(trace);

    Command::new(word)
        .version(env!("CARGO_PKG_VERSION"))
        .about(about)
        .subcommand_required(true)
        .arg_required_else_help(false)
        .subcommand(trace)
}

/// Builds the `agent` word's clap tree.
#[must_use]
pub fn agent_command() -> Command {
    let stats = scope_arguments(
        Command::new("stats")
            .about("One object of this agent's own numbers over the window")
            .arg(
                Arg::new("agent")
                    .long("agent")
                    .value_name("ID")
                    .required(true)
                    .help(
                        "Whose numbers. Required in 0.1.0: the dekopon:meta/caller import that \
                         would make the scope a fact of the invocation does not exist yet, so a \
                         grant of this capability can name any agent",
                    ),
            ),
    );
    Command::new(AGENT_WORD)
        .version(env!("CARGO_PKG_VERSION"))
        .about("An agent's own turns, tokens, latency, capability calls, and denials")
        .subcommand_required(true)
        .subcommand(stats)
}

/// Builds the `broker` word's clap tree.
#[must_use]
pub fn broker_command() -> Command {
    let providers = scope_arguments(Command::new("providers").about(
        "What the broker loaded at its last boot inside the window: id, digest, counts, compile time",
    ));
    let mut usage = scope_arguments(
        Command::new("usage")
            .about("Calls over the window, grouped")
            .arg(
                Arg::new("by")
                    .long("by")
                    .value_name("KEY")
                    .value_parser(["provider", "capability", "agent"])
                    .default_value("provider")
                    .help("What to group by"),
            ),
    );
    usage = limit_argument(usage);
    let mut denials = scope_arguments(
        Command::new("denials").about("Refusals over the window, grouped by reason and capability"),
    );
    denials = limit_argument(denials);

    Command::new(BROKER_WORD)
        .version(env!("CARGO_PKG_VERSION"))
        .about("The fleet view: what is loaded, what was called, what was refused")
        .subcommand_required(true)
        .subcommand(providers)
        .subcommand(usage)
        .subcommand(denials)
        .arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .hide(true)
                .help("Reserved"),
        )
}

/// Runs one raw-word argv.
///
/// # Errors
///
/// Returns a usage refusal for a window outside the cap or a malformed trace ID.
pub fn run_raw(
    word: &'static str,
    about: &'static str,
    capabilities: &Capabilities,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<CommandRun, ProviderError> {
    cli::run_command(raw_command(word, about), argv, stdin, |matches, _stdin| {
        dispatch_raw(capabilities, &matches)
    })
}

/// Runs one `agent` argv.
///
/// # Errors
///
/// Returns the decline when the argv parsed but named something this provider refuses.
pub fn run_agent(
    capabilities: &Capabilities,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<CommandRun, ProviderError> {
    cli::run_command(agent_command(), argv, stdin, |matches, _stdin| {
        let (_, sub) = matches
            .subcommand()
            .ok_or_else(|| usage("agent: no action"))?;
        let scope = scope_from(sub)?;
        Ok(CommandInvocation {
            capability: capabilities.agent_stats.clone(),
            input: merge(
                scope,
                json!({"agent": string(sub, "agent").unwrap_or_default()}),
            ),
            secret_use: None,
        })
    })
}

/// Runs one `broker` argv.
///
/// # Errors
///
/// Returns the decline when the argv parsed but named something this provider refuses.
pub fn run_broker(
    capabilities: &Capabilities,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<CommandRun, ProviderError> {
    cli::run_command(broker_command(), argv, stdin, |matches, _stdin| {
        let (name, sub) = matches
            .subcommand()
            .ok_or_else(|| usage("broker: no action"))?;
        let scope = scope_from(sub)?;
        let (capability, view) = match name {
            "providers" => (&capabilities.broker_providers, BrokerView::Providers),
            "usage" => (&capabilities.broker_usage, BrokerView::Usage),
            "denials" => (&capabilities.broker_usage, BrokerView::Denials),
            other => return Err(usage(format!("broker {other}: unknown action"))),
        };
        let mut extra = json!({"view": view});
        if view != BrokerView::Providers {
            extra["limit"] = json!(limit(sub));
        }
        if view == BrokerView::Usage {
            let by = match string(sub, "by")
                .unwrap_or_else(|| "provider".to_owned())
                .as_str()
            {
                "capability" => UsageGrouping::Capability,
                "agent" => UsageGrouping::Agent,
                _ => UsageGrouping::Provider,
            };
            extra["by"] = json!(by);
        }
        Ok(CommandInvocation {
            capability: capability.clone(),
            input: merge(scope, extra),
            secret_use: None,
        })
    })
}

fn dispatch_raw(
    capabilities: &Capabilities,
    matches: &ArgMatches,
) -> Result<CommandInvocation, ProviderError> {
    let (name, sub) = matches
        .subcommand()
        .ok_or_else(|| usage("no action was named"))?;
    if name != "trace" {
        return Err(usage(format!("unknown action {name}")));
    }
    Ok(CommandInvocation {
        capability: capabilities.trace.clone(),
        input: merge(
            scope_from(sub)?,
            json!({
                "traceId": string(sub, "trace-id").unwrap_or_default(),
                "limit": limit(sub),
            }),
        ),
        secret_use: None,
    })
}

fn scope_from(matches: &ArgMatches) -> Result<Value, ProviderError> {
    let since_seconds = parse_since(&string(matches, "since").unwrap_or_default())
        .map_err(|error| usage(error.to_string()))?;
    let max_output_bytes = matches
        .get_one::<u64>("max-output-bytes")
        .and_then(|value| usize::try_from(*value).ok())
        .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
    if !(MIN_OUTPUT_BYTES..=MAX_OUTPUT_BYTES_CEILING).contains(&max_output_bytes) {
        return Err(usage("max-output-bytes is outside the provider's bounds"));
    }
    Ok(json!({
        "sinceSeconds": since_seconds,
        "maxOutputBytes": max_output_bytes,
        "format": string(matches, "format").unwrap_or_else(|| "json".to_owned()),
    }))
}

fn merge(mut scope: Value, extra: Value) -> Value {
    if let (Some(target), Some(source)) = (scope.as_object_mut(), extra.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    scope
}

fn string(matches: &ArgMatches, name: &str) -> Option<String> {
    matches.get_one::<String>(name).cloned()
}

fn limit(matches: &ArgMatches) -> u32 {
    matches
        .get_one::<u32>("limit")
        .copied()
        .unwrap_or(DEFAULT_LIMIT)
}

fn usage(message: impl std::fmt::Display) -> ProviderError {
    ProviderError::new("usage", message.to_string())
}

/// The `--since` cap, as `--help` states it, for a backend's own documentation tests.
#[must_use]
pub fn window_cap_seconds() -> u64 {
    MAX_WINDOW_SECONDS
}

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{CommandInvocation, CommandRun};

    use super::{Capabilities, Route, overview, route, run_agent, run_broker, run_raw};

    fn ids() -> Capabilities {
        Capabilities::for_provider("openobserve")
    }
    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }
    fn proposed(run: CommandRun) -> CommandInvocation {
        match run {
            CommandRun::Proposal(invocation) => invocation,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn only_four_generated_capabilities_are_declared() {
        let ids = ids();
        assert_eq!(
            ids.all().map(|id| id.as_str().to_owned()),
            [
                "openobserve.trace",
                "openobserve.agent-stats",
                "openobserve.broker-providers",
                "openobserve.broker-usage",
            ]
        );
        assert_eq!(route(&argv(&["trace"])), Route::Raw);
        assert_eq!(route(&argv(&["sql"])), Route::Overview);
        assert_eq!(route(&argv(&["search"])), Route::Overview);
    }

    #[test]
    fn model_facing_help_and_commands_have_no_sql_or_destination_controls() {
        let help = overview("openobserve", "Telemetry");
        for forbidden in [
            " sql ", " search ", "--url", "--org", "--stream", "--where", "--select",
        ] {
            assert!(!help.contains(forbidden), "{forbidden}");
        }
        for action in ["sql", "search"] {
            let run = run_raw(
                "openobserve",
                "Telemetry",
                &ids(),
                &argv(&[action, "--since", "1h"]),
                None,
            )
            .unwrap();
            assert!(matches!(run, CommandRun::Rendered { .. }), "{action}");
        }
        let run = run_raw(
            "openobserve",
            "Telemetry",
            &ids(),
            &argv(&[
                "trace",
                "--since",
                "1h",
                "--url",
                "https://elsewhere.example",
                "0af7651916cd43dd8448eb211c80319c",
            ]),
            None,
        )
        .unwrap();
        assert!(matches!(run, CommandRun::Rendered { .. }));
    }

    #[test]
    fn generated_proposals_have_only_bounded_agent_fields() {
        let trace = proposed(
            run_raw(
                "openobserve",
                "Telemetry",
                &ids(),
                &argv(&["trace", "--since", "1h", "0af7651916cd43dd8448eb211c80319c"]),
                None,
            )
            .unwrap(),
        );
        assert_eq!(trace.capability.as_str(), "openobserve.trace");
        assert_eq!(trace.input["sinceSeconds"], 3600);
        assert!(trace.input.get("url").is_none());
        assert!(trace.input.get("org").is_none());
        assert!(trace.input.get("stream").is_none());
        let stats = proposed(
            run_agent(
                &ids(),
                &argv(&["stats", "--since", "1h", "--agent", "xavier"]),
                None,
            )
            .unwrap(),
        );
        assert_eq!(stats.input["agent"], "xavier");
        let usage = proposed(
            run_broker(
                &ids(),
                &argv(&["usage", "--since", "1h", "--by", "capability"]),
                None,
            )
            .unwrap(),
        );
        assert_eq!(usage.input["by"], "capability");
        assert_eq!(usage.input["view"], "usage");
    }
}
