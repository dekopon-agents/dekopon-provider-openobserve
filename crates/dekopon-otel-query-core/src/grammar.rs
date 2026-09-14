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
//! are the same words against any store that speaks dekopon's record schema, and the raw word is
//! the only one whose *name* is the backend's. So the raw tree takes its name at build time and the
//! capability ids arrive as [`Capabilities`], which a backend builds from its own provider id.

use dekopon_provider_sdk::clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use dekopon_provider_sdk::{CapabilityId, CommandInvocation, CommandRun, ProviderError, cli};
use serde_json::{Value, json};

use crate::query::{
    BrokerView, DEFAULT_LIMIT, DEFAULT_ORG, DEFAULT_STREAM, Format, MAX_LIMIT, QueryError, Scope,
    Signal, UsageGrouping, check_statement,
};
use crate::window::MAX_WINDOW_SECONDS;

/// The capability ids one backend's words propose.
///
/// Ids follow the provider — `openobserve.agent-stats`, `quickwit.agent-stats` — so an owner can
/// grant the fleet view over dekopon's own store without granting it over someone else's, and the
/// audit record names which store was read.
#[derive(Clone, Debug)]
pub struct Capabilities {
    /// `<backend> sql` and `<backend> search`.
    pub search: CapabilityId,
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
    /// The five ids for one provider id.
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
            search: id("search"),
            trace: id("trace"),
            agent_stats: id("agent-stats"),
            broker_providers: id("broker-providers"),
            broker_usage: id("broker-usage"),
        }
    }

    /// Every id, in manifest order.
    #[must_use]
    pub fn all(&self) -> [&CapabilityId; 5] {
        [
            &self.search,
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
/// It recovers cleanly because the three vocabularies are disjoint by construction: `sql`, `search`
/// and `trace` belong to the raw word, `stats` to `agent`, and `providers`, `usage` and `denials`
/// to `broker`. What cannot be recovered is a bare `--help` or an empty argv, which are identical
/// for all three — so those render one overview naming every word and every action, which is more
/// useful than one word's page chosen by a coin flip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    /// `sql`, `search`, `trace`.
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
        Some("sql" | "search" | "trace") => Route::Raw,
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
         \x20 {raw_word} sql --url <URL> --since <DURATION> [--type traces|logs] [--limit <N>] <SQL>\n\
         \x20 {raw_word} search --url <URL> --since <DURATION> [--where <PREDICATE>] [--select <COLUMNS>]\n\
         \x20 {raw_word} trace --url <URL> --since <DURATION> <TRACE-ID>\n\
         \x20 agent stats --url <URL> --since <DURATION> --agent <ID>\n\
         \x20 broker providers --url <URL> --since <DURATION>\n\
         \x20 broker usage --url <URL> --since <DURATION> [--by provider|capability|agent]\n\
         \x20 broker denials --url <URL> --since <DURATION>\n\n\
         Every word takes --format json|table. json is the default and is one object with a rows\n\
         array, so `<word> … | jq '.rows[] | .duration_ms'` works with no wrapper; row keys are the\n\
         store's own column names. table renders a fixed-width text table instead.\n\n\
         --since is capped at 30d and --limit at 500. Attribute names are folded to letters, digits\n\
         and underscore: audit.event is the column audit_event.\n\n\
         Try `{raw_word} sql --help`, `agent stats --help`, or `broker usage --help`.\n"
    )
}

const SINCE_HELP: &str = "How far back to look: 30m, 24h, 7d (units s, m, h, d). Capped at 30d, \
                          which is the homelab store's retention";

fn scope_arguments(command: Command) -> Command {
    command
        .arg(
            Arg::new("url")
                .long("url")
                .value_name("URL")
                .required(true)
                .help("The store's base URL, e.g. https://rpi.lan/openobserve"),
        )
        .arg(
            Arg::new("org")
                .long("org")
                .value_name("ORG")
                .default_value(DEFAULT_ORG)
                .help("The store's organization"),
        )
        .arg(
            Arg::new("stream")
                .long("stream")
                .value_name("STREAM")
                .default_value(DEFAULT_STREAM)
                .help("The stream both dekopon daemons export into"),
        )
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
    let mut sql = scope_arguments(
        Command::new("sql")
            .about("Run one read-only statement and return its rows")
            .arg(Arg::new("statement").value_name("SQL").required(true).help(
                "One statement, starting with SELECT (or a WITH that ends in one). \
                         Attribute names are folded: audit.event is the column audit_event",
            )),
    );
    sql = limit_argument(sql).arg(signal_argument());

    let mut search = scope_arguments(
        Command::new("search")
            .about("Assemble one filtered SELECT over the stream and return its rows")
            .arg(
                Arg::new("where")
                    .long("where")
                    .value_name("PREDICATE")
                    .help("A SQL predicate over folded column names, e.g. operation_name = 'gateway.session'"),
            )
            .arg(
                Arg::new("select")
                    .long("select")
                    .value_name("COLUMNS")
                    .help("Comma-separated columns to project. Defaults to every column"),
            ),
    );
    search = limit_argument(search).arg(signal_argument());

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
        .subcommand(sql)
        .subcommand(search)
        .subcommand(trace)
}

fn signal_argument() -> Arg {
    Arg::new("type")
        .long("type")
        .value_name("SIGNAL")
        .value_parser(["traces", "logs"])
        .default_value("traces")
        .help("Which signal to search; spans or log records")
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
/// Returns the decline when the argv parsed but named something this provider refuses — a window
/// past the cap, a statement that is not a read, a malformed URL.
pub fn run_raw(
    word: &'static str,
    about: &'static str,
    capabilities: &Capabilities,
    argv: &[String],
    stdin: Option<&str>,
) -> Result<CommandRun, ProviderError> {
    cli::run_command(raw_command(word, about), argv, stdin, |matches, stdin| {
        dispatch_raw(capabilities, &matches, stdin)
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
    _stdin: Option<&str>,
) -> Result<CommandInvocation, ProviderError> {
    let (name, sub) = matches
        .subcommand()
        .ok_or_else(|| usage("no action was named"))?;
    let scope = scope_from(sub)?;
    match name {
        "sql" => {
            let statement = string(sub, "statement").unwrap_or_default();
            let sql = check_statement(&statement).map_err(decline)?;
            Ok(CommandInvocation {
                capability: capabilities.search.clone(),
                input: merge(
                    scope,
                    json!({"signal": signal(sub), "sql": sql, "limit": limit(sub)}),
                ),
                secret_use: None,
            })
        }
        "search" => {
            let sql = assemble(
                &string(sub, "stream").unwrap_or_else(|| DEFAULT_STREAM.to_owned()),
                string(sub, "select").as_deref(),
                string(sub, "where").as_deref(),
                limit(sub),
            )
            .map_err(decline)?;
            Ok(CommandInvocation {
                capability: capabilities.search.clone(),
                input: merge(
                    scope,
                    json!({"signal": signal(sub), "sql": sql, "limit": limit(sub)}),
                ),
                secret_use: None,
            })
        }
        "trace" => Ok(CommandInvocation {
            capability: capabilities.trace.clone(),
            input: merge(
                scope,
                json!({
                    "traceId": string(sub, "trace-id").unwrap_or_default(),
                    "limit": limit(sub),
                }),
            ),
            secret_use: None,
        }),
        other => Err(usage(format!("unknown action {other}"))),
    }
}

/// Assembles `search`'s statement, which is the only SQL this provider writes for a caller.
///
/// The predicate is passed through as the caller wrote it — this is the Medium-risk capability, and
/// a predicate parser is a non-goal — but the projection is checked, because a column list is a
/// place a second statement would otherwise hide.
fn assemble(
    stream: &str,
    select: Option<&str>,
    predicate: Option<&str>,
    limit: u32,
) -> Result<String, QueryError> {
    let projection = match select {
        None => "*".to_owned(),
        Some(columns) => {
            let names: Vec<&str> = columns.split(',').map(str::trim).collect();
            for name in &names {
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(QueryError::invalid(format!(
                        "--select {name}: expected a column name of letters, digits, and underscore"
                    )));
                }
            }
            names.join(", ")
        }
    };
    let mut sql = format!("SELECT {projection} FROM \"{stream}\"");
    if let Some(predicate) = predicate.map(str::trim).filter(|value| !value.is_empty()) {
        if predicate.contains(';') {
            return Err(QueryError::invalid(
                "--where must be one predicate; `;` is refused",
            ));
        }
        sql.push_str(" WHERE ");
        sql.push_str(predicate);
    }
    sql.push_str(&format!(" ORDER BY _timestamp DESC LIMIT {limit}"));
    check_statement(&sql)
}

fn scope_from(matches: &ArgMatches) -> Result<Value, ProviderError> {
    let scope = Scope::from_flags(
        string(matches, "url").unwrap_or_default(),
        string(matches, "org").unwrap_or_else(|| DEFAULT_ORG.to_owned()),
        string(matches, "stream").unwrap_or_else(|| DEFAULT_STREAM.to_owned()),
        &string(matches, "since").unwrap_or_default(),
        matches
            .get_one::<u64>("max-output-bytes")
            .and_then(|value| usize::try_from(*value).ok())
            .unwrap_or(crate::fit::DEFAULT_MAX_OUTPUT_BYTES),
        match string(matches, "format").as_deref() {
            Some("table") => Format::Table,
            _ => Format::Json,
        },
    )
    .map_err(decline)?;
    serde_json::to_value(scope).map_err(|error| usage(error.to_string()))
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

fn signal(matches: &ArgMatches) -> Signal {
    match string(matches, "type").as_deref() {
        Some("logs") => Signal::Logs,
        _ => Signal::Traces,
    }
}

fn usage(message: impl std::fmt::Display) -> ProviderError {
    ProviderError::new("usage", message.to_string())
}

fn decline(error: QueryError) -> ProviderError {
    ProviderError::new("usage", error.message().to_owned())
}

/// The `--since` cap, as `--help` states it, for a backend's own documentation tests.
#[must_use]
pub fn window_cap_seconds() -> u64 {
    MAX_WINDOW_SECONDS
}

#[cfg(test)]
mod tests {
    use dekopon_provider_sdk::{CommandInvocation, CommandRun};

    use super::{Capabilities, assemble, run_agent, run_broker, run_raw};

    const WORD: &str = "openobserve";
    const ABOUT: &str = "Query an OpenObserve telemetry store";

    fn capabilities() -> Capabilities {
        Capabilities::for_provider("openobserve")
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn rendered(run: CommandRun) -> (String, String, u8) {
        match run {
            CommandRun::Rendered {
                stdout,
                stderr,
                status,
            } => (stdout, stderr, status),
            other => panic!("expected rendered text, got {other:?}"),
        }
    }

    fn proposal(run: CommandRun) -> CommandInvocation {
        match run {
            CommandRun::Proposal(invocation) => invocation,
            other => panic!("expected a proposal, got {other:?}"),
        }
    }

    fn raw(words: &[&str]) -> CommandRun {
        run_raw(WORD, ABOUT, &capabilities(), &argv(words), None)
            .expect("clap answers are rendered")
    }

    #[test]
    fn the_five_capability_ids_follow_the_provider_id() {
        let ids: Vec<String> = capabilities()
            .all()
            .iter()
            .map(|id| id.as_str().to_owned())
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
        assert_eq!(
            Capabilities::for_provider("quickwit").agent_stats.as_str(),
            "quickwit.agent-stats"
        );
    }

    #[test]
    fn help_renders_on_stdout_at_status_zero_for_every_word() {
        let (stdout, stderr, status) = rendered(raw(&["--help"]));
        assert_eq!(status, 0);
        assert!(stdout.contains("Usage: openobserve <COMMAND>"), "{stdout}");
        for action in ["sql", "search", "trace"] {
            assert!(stdout.contains(action), "{stdout}");
        }
        assert!(stderr.is_empty());

        let (stdout, _, status) = rendered(
            run_agent(&capabilities(), &argv(&["stats", "--help"]), None).expect("rendered"),
        );
        assert_eq!(status, 0);
        assert!(stdout.contains("Usage: agent stats"), "{stdout}");
        // The cap is on the help page, which is where a model learns it without spending a call.
        assert!(stdout.contains("Capped at 30d"), "{stdout}");

        let (stdout, _, status) =
            rendered(run_broker(&capabilities(), &argv(&["--help"]), None).expect("rendered"));
        assert_eq!(status, 0);
        for action in ["providers", "usage", "denials"] {
            assert!(stdout.contains(action), "{stdout}");
        }
    }

    #[test]
    fn a_well_formed_argv_proposes_the_capability_the_manifest_declares() {
        let invocation = proposal(raw(&[
            "sql",
            "--url",
            "https://rpi.lan/openobserve",
            "--since",
            "24h",
            "--limit",
            "50",
            "SELECT trace_id FROM \"dekopon\"",
        ]));
        assert_eq!(invocation.capability.as_str(), "openobserve.search");
        assert_eq!(invocation.input["url"], "https://rpi.lan/openobserve");
        assert_eq!(invocation.input["org"], "default");
        assert_eq!(invocation.input["stream"], "dekopon");
        assert_eq!(invocation.input["sinceSeconds"], 86_400);
        assert_eq!(invocation.input["format"], "json");
        assert_eq!(invocation.input["signal"], "traces");
        assert_eq!(invocation.input["limit"], 50);
        assert_eq!(invocation.input["sql"], "SELECT trace_id FROM \"dekopon\"");

        let invocation = proposal(
            run_agent(
                &capabilities(),
                &argv(&[
                    "stats",
                    "--url",
                    "https://rpi.lan/openobserve",
                    "--agent",
                    "reviewer",
                    "--since",
                    "7d",
                ]),
                None,
            )
            .expect("a proposal"),
        );
        assert_eq!(invocation.capability.as_str(), "openobserve.agent-stats");
        assert_eq!(invocation.input["agent"], "reviewer");
        assert_eq!(invocation.input["sinceSeconds"], 604_800);

        let invocation = proposal(
            run_broker(
                &capabilities(),
                &argv(&[
                    "usage",
                    "--url",
                    "https://rpi.lan/openobserve",
                    "--since",
                    "24h",
                    "--by",
                    "agent",
                ]),
                None,
            )
            .expect("a proposal"),
        );
        assert_eq!(invocation.capability.as_str(), "openobserve.broker-usage");
        assert_eq!(invocation.input["view"], "usage");
        assert_eq!(invocation.input["by"], "agent");

        let invocation = proposal(
            run_broker(
                &capabilities(),
                &argv(&[
                    "denials",
                    "--url",
                    "https://rpi.lan/openobserve",
                    "--since",
                    "24h",
                ]),
                None,
            )
            .expect("a proposal"),
        );
        assert_eq!(invocation.capability.as_str(), "openobserve.broker-usage");
        assert_eq!(invocation.input["view"], "denials");
    }

    /// The window cap is enforced in the word, so a 90-day request never becomes a proposal, never
    /// reaches Cedar, and never spends a request.
    #[test]
    fn a_window_past_the_cap_is_a_usage_error_not_a_proposal() {
        let error = run_raw(
            WORD,
            ABOUT,
            &capabilities(),
            &argv(&[
                "sql",
                "--url",
                "https://rpi.lan/openobserve",
                "--since",
                "90d",
                "SELECT 1",
            ]),
            None,
        )
        .expect_err("the cap holds");
        assert_eq!(error.code(), "usage");
        assert!(error.message().contains("30d"), "{}", error.message());
    }

    #[test]
    fn usage_errors_render_on_stderr_at_status_two() {
        for words in [
            &[][..],
            &["bogus"][..],
            &["sql"][..],
            &["sql", "--url", "https://rpi.lan/openobserve", "SELECT 1"][..],
            &["sql", "--since", "24h", "SELECT 1"][..],
            &[
                "sql",
                "--url",
                "https://rpi.lan/openobserve",
                "--since",
                "24h",
                "--limit",
                "501",
                "SELECT 1",
            ][..],
            &[
                "sql",
                "--url",
                "https://rpi.lan/openobserve",
                "--since",
                "24h",
                "--type",
                "metrics",
                "SELECT 1",
            ][..],
        ] {
            let (stdout, stderr, status) = rendered(raw(words));
            assert_eq!(status, 2, "{words:?}");
            assert!(stdout.is_empty(), "{words:?}: {stdout}");
            assert!(!stderr.is_empty(), "{words:?}");
        }
    }

    /// A statement that is not a read is refused in the word, before anything is proposed.
    #[test]
    fn a_write_statement_never_becomes_a_proposal() {
        for statement in ["DELETE FROM \"dekopon\"", "SELECT 1; DROP TABLE x"] {
            let error = run_raw(
                WORD,
                ABOUT,
                &capabilities(),
                &argv(&[
                    "sql",
                    "--url",
                    "https://rpi.lan/openobserve",
                    "--since",
                    "24h",
                    statement,
                ]),
                None,
            )
            .expect_err("refused");
            assert_eq!(error.code(), "usage", "{statement}");
        }
    }

    #[test]
    fn search_assembles_exactly_one_ordered_bounded_statement() {
        assert_eq!(
            assemble("dekopon", None, None, 100).expect("assembled"),
            "SELECT * FROM \"dekopon\" ORDER BY _timestamp DESC LIMIT 100"
        );
        assert_eq!(
            assemble(
                "dekopon",
                Some("trace_id, operation_name"),
                Some("operation_name = 'gateway.session'"),
                5
            )
            .expect("assembled"),
            "SELECT trace_id, operation_name FROM \"dekopon\" WHERE operation_name = \
             'gateway.session' ORDER BY _timestamp DESC LIMIT 5"
        );
        for select in ["trace_id; DROP TABLE x", "trace_id, (SELECT 1)", ""] {
            assert!(
                assemble("dekopon", Some(select), None, 5).is_err(),
                "{select} was accepted"
            );
        }
        assert!(assemble("dekopon", None, Some("a = 1; DELETE FROM x"), 5).is_err());
    }

    /// The proposal a word makes is the input a direct `cap` call would send, so the two paths
    /// cannot drift: the keys are camelCase on both.
    #[test]
    fn every_proposal_carries_only_camel_case_keys() {
        let invocation = proposal(raw(&[
            "trace",
            "--url",
            "https://rpi.lan/openobserve",
            "--since",
            "1h",
            "0af7651916cd43dd8448eb211c80319c",
        ]));
        assert_eq!(invocation.capability.as_str(), "openobserve.trace");
        assert_eq!(
            invocation.input["traceId"],
            "0af7651916cd43dd8448eb211c80319c"
        );
        for key in invocation.input.as_object().expect("an object").keys() {
            assert!(!key.contains('_'), "{key} is snake_case");
        }
    }
}
