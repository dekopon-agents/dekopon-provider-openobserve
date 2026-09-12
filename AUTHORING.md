# Authoring log

What was decided while building this, and why — the parts that would otherwise have to be
rediscovered from the diff.

## 2026-09-12 — contract

Built from [dekopon#250](https://github.com/dekopon-agents/dekopon/issues/250), which absorbed #248
(an agent's own stats) and #249 (the broker meta provider, which does not exist). The design was
taken as given; three things in it turned out not to survive contact with the tree.

**`accounting.model.turn` is unreachable as a column.** #250 plans `agent stats` against that
record. `dekopond` builds only `telemetry.settings.tracer_provider()` — `optional_logger_provider`
is wired in `dekopon-brokerd` since #217 and nowhere else — so the record is a `tracing` event
inside a span, `tracing_opentelemetry` turns it into a span event, and OpenObserve serializes a
span's events into one `events` **string** column (`Span { …, events: String }`,
`src/common/src/meta/traces.rs`). `docs/improvement.md`'s `SELECT * FROM "dekopon" WHERE audit_event
= '…'` works for brokerd's records, which are real log rows, and not for dekopond's.

The fix cost nothing: `record_usage` writes the identical five token counts onto the enclosing
`prompt.model_turn` **span**, which folds to ordinary columns, and OpenObserve's own `duration`
column is the turn's latency. So `agent stats` reads the span. What was lost is the turn's
`outcome`, which exists only on the event — `span_status` is `Unset` for a successful turn and for
most failures — so `succeeded`/`failed` are not measured and the README says so rather than
emitting a number that looks real.

**`plan` needs the prior answers.** #250 specifies `fn plan(Word, Window) -> Vec<HttpRequest>`. The
agent id is on `gateway.session` and the token counts are on the `prompt.model_turn` spans beneath
it, joined by `trace_id`, so the second statement's `IN (…)` is not knowable until the first answer
is in hand. The trait is `plan(&Query, prior: &[Response]) -> Option<Request>`, driven by a loop
with a hard ceiling of four steps. A subquery would have kept the flat signature and bet the word on
which DataFusion features the deployed store exposes.

**Three command words, one argv, no word.** `dekopon:provider@0.3.0` declares `run-command:
func(argv: list<string>, stdin: option<string>)`. The broker host resolves the provider *by* the
word (`BrokerHost::run_command(word, argv, stdin)`) and then hands the guest `argv` alone. A
one-word provider never notices; this one has three. The three action vocabularies are disjoint by
construction, so the action recovers the word — and a bare `--help`, `--version`, or empty argv
cannot, so those render one overview page naming every word and action. That is arguably better
than one word's page chosen arbitrarily, but it is a workaround for a gap in the export signature,
and the honest fix upstream is a `word` parameter on `run-command`.

## 2026-09-12 — implementation

**Two crates from the start, not extracted later.** #250's Backends section is explicit that
Quickwit is the second component over one core, and the core's boundary is only testable if
something is on the other side of it. So `dekopon-otel-query-core` holds the grammar, the window
cap, the projection, the fitting, and the output serializers, and knows nothing about `_search`; the
provider crate holds `plan`, `fold`, and the SQL. `Capabilities::for_provider("openobserve")` is how
capability ids follow the provider without the core hard-coding one. The gate builds the core crate
for `wasm32-unknown-unknown` on its own, so the second repository's first build is not where a
native-only dependency is discovered.

**camelCase on the wire, both directions.** The shell rewrites `--kebab-flags` into camelCase JSON
keys before calling a capability, so `dekopon-provider-mediawiki` v0.1.0's snake_case fields with
`deny_unknown_fields` could never be reached through the flag form. Every input field and every
output key here is camelCase, and a test asserts no proposal and no output carries an underscore.

**The exclusion list is mechanized, not prose.** `projection::FORBIDDEN_COLUMNS` plus four prefix
families, and `planned_columns()` enumerates every column any statement in the backend names. One
test runs the second through the first. A statement that grows a column fails that test rather than
shipping a leak. `openobserve sql`/`search` are deliberately exempt: they are Medium risk and
operator-only precisely because they return whatever is selected, and projecting them would be a
bound a caller walks around with `SELECT *` — a bound that can be walked around is worse than an
honest capability boundary.

**`events` is never selected.** For dekopon that string carries `agent.model.prompt` and
`agent.model.answer`: the whole conversation. `openobserve trace` returns the span skeleton only,
which is also why every column it names is a non-optional field of OpenObserve's own span record —
a projection that named an attribute a particular deployment never ingested would 400 rather than
return fewer columns.

**`approx_percentile_cont`, server-side.** The alternative was folding over a page of rows, which
makes a token total a sample. It was taken only after finding the function in OpenObserve's own
service-graph SQL (`src/core/src/traces/service_graph/processor.rs`), so it is a function the
deployed store demonstrably has rather than one a docs page lists.

**200 sessions is a request-body budget, not a taste.** `trace_id IN (…)` at 200 quoted ids is about
7 KiB, inside the 16 KiB `maxRequestBytes` #250's constraint sets write. A test asserts the widest
statement fits.

## Validation record

`./scripts/validate.sh` green on macOS arm64 with Rust 1.98.1 and wasm-tools 1.259.0: MSRV check,
fmt, 66 tests, clippy on both targets with `-D warnings`, the three WIT mirrors against the crates
Cargo resolved, no ambient dependency, no hand-written `unsafe`, exactly two core imports
(`dekopon:http/client@1.0.0`, `dekopon:clock/wall@1.0.0`), exactly three exports, no local path in
the artifact, and the reproducible build's digest matching its sidecar.

Not run: any call against a live OpenObserve. The homelab API is `http://rpi.lan/openobserve` and
the native HTTP host refuses plaintext to non-loopback destinations, so the first real call waits on
TLS or on dekopon's broker-level plaintext allowlist. The fixtures are built to the documented
`_search` response shape and OpenObserve's own span record, not captured.
