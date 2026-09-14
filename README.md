# dekopon-provider-openobserve

A bounded, read-only client for an OpenObserve telemetry store, as a Dekopon Wasm component.
Designed in [dekopon#250](https://github.com/dekopon-agents/dekopon/issues/250).

Goal 2 says everything that happened is in the operator's telemetry store. This is how an owner
grants a model a bounded read of it: an ordinary `dekopon:http@1.0.0` client with a broker-injected,
DRN-bound Basic credential, no endpoint of its own, and no authority it did not receive. It replaces
something that was deleted rather than adding something new — `dekopon-run session list | show |
replay` read sessions back from OpenObserve and went with the runner in 0.13.0.

Two crates, because #250's Backends section decided that shape:

| Crate | What it owns |
|---|---|
| `dekopon-otel-query-core` | The command-word grammar, the 30-day window cap, the column projection and its exclusion list, the `truncated`/`omittedRows` fitting, and the output shapes. Backend-neutral. |
| `dekopon-openobserve-provider` | The component: `plan` and `fold` against OpenObserve's `_search`, and the manifest. |

A later `dekopon-provider-quickwit` depends on the core crate rather than forking it. The core
crate is not published to crates.io today — nothing outside this workspace consumes it, and a
published crate with no consumer is a maintenance claim nobody asked for.

## Capabilities

| Capability | Effect | Risk | Words | `_search` calls |
|---|---|---|---|---|
| `openobserve.search` | read-only | Medium | `openobserve sql`, `openobserve search` | 1 |
| `openobserve.trace` | read-only | Low | `openobserve trace` | 1 |
| `openobserve.agent-stats` | read-only | Low | `agent stats` | 2–3 |
| `openobserve.broker-providers` | read-only | Low | `broker providers` | 1–2 |
| `openobserve.broker-usage` | read-only | Low | `broker usage`, `broker denials` | 1 |

`openobserve.search` is the only Medium one and the split is the whole design: it returns whatever
the statement selects, transcripts included, so it goes to an operator's own agent. The other four
return an explicit column list that never contains conversation text, a principal, a policy id, a
credential name, or a path — `inspect_agent_config`'s `omitted` list applied to a stats view, and
mechanized as a test over every statement this component can write.

## The three command words

`openobserve` is named after the backend so an operator can load this provider for dekopon's own
store and a Quickwit one for somebody else's, side by side; command words are global per broker and
a duplicate fails startup. `agent` and `broker` are backend-neutral and are claimed only by
whichever provider speaks dekopon's record schema.

```
openobserve sql --url <URL> --since <DURATION> [--type traces|logs] [--limit <N>] <SQL>
openobserve search --url <URL> --since <DURATION> [--where <PREDICATE>] [--select <COLUMNS>]
openobserve trace --url <URL> --since <DURATION> <TRACE-ID>
agent stats --url <URL> --since <DURATION> --agent <ID>
broker providers --url <URL> --since <DURATION>
broker usage --url <URL> --since <DURATION> [--by provider|capability|agent]
broker denials --url <URL> --since <DURATION>
```

**`--help` on its own renders one page for all three words, not one word's page.** The component is
never told which word was typed: `dekopon:provider@0.3.0` declares
`run-command: func(argv, stdin)`, and the broker host resolves the provider *by* the word and then
hands the guest the remaining argv alone. The three action vocabularies are disjoint — `sql`,
`search`, `trace` | `stats` | `providers`, `usage`, `denials` — so an action recovers its word;
a bare `--help` cannot, and renders an overview naming every word and action instead. `agent stats
--help` reaches the real page.

**Two flag conventions, and the model sees both.** Through a command word the flags are this
provider's own, kebab-case as clap renders them: `--max-output-bytes`, `--trace-id` as a
positional. Through `cap openobserve.agent-stats {…}` the keys are camelCase: `maxOutputBytes`,
`traceId`, `sinceSeconds`. The two agree on purpose — the sandboxed shell rewrites `--kebab-flags`
into camelCase JSON keys before calling a capability, so a provider declaring snake_case fields with
`deny_unknown_fields` can never be called through the flag form. That is the bug
`dekopon-provider-mediawiki` v0.1.0 shipped, and this component's wire is camelCase in both
directions to avoid it. A test asserts no proposal carries a key with an underscore.

## Two output formats, and why the default is the pipeable one

```sh
openobserve sql --url ... --since 1h 'SELECT operation_name, duration FROM "dekopon"' \
  | jq '.rows[] | .duration_ms'
agent stats --url ... --agent reviewer --since 24h --format table
```

Every word takes `--format json|table`, and `json` is the default.

**`json` is one object, not JSON Lines.** The shell's `jq` builtin takes the piped command's
*value* — `run(&self, ..., input: Option<Value>)`, handed straight to the jaq interpreter — so a
capability that returns one JSON object is already composable with no wrapper and no parse step.
JSON Lines would have to be reassembled by whoever consumed it, and the one consumer that matters
here never sees text. So: one object, `{"rows":[...],"returned":n,"total":t,"truncated":b,"omittedRows":k}`,
for every word that returns rows.

**Row keys are the store's, envelope keys are #250's.** A row's keys are the folded column names
exactly as OpenObserve returned them — `duration_ms`, `capability_id`, `operation_name`,
`usage_input_tokens` — passed through untouched, because renaming them would break the filter a
model wrote against a `SELECT` it typed itself. The envelope and the `agent stats` statistic keys
are camelCase (`omittedRows`, `turns.durationMs.p95`, `tokens.cachedInput`) because #250 specifies
that shape byte for byte as the contract every backend must produce. The two conventions meet at the
envelope boundary and neither moves.

**`table` is a fixed-width text table** for a model to read directly rather than filter. Columns come
from the rows' keys in the parsed object's order, which is alphabetical and therefore stable across
two answers to the same question; a cell wider than 48 characters is elided with an ellipsis and
newlines are flattened, so no value can break the shape. The truncation marker is a footer line:

```text
duration_ms  operation_name     trace_id
-----------  -----------------  --------------------------------
41           gateway.session    0af7651916cd43dd8448eb211c80319c
2140         prompt.model_turn  1bf7651916cd43dd8448eb211c80319d
-- 2 of 1873 rows; truncated, 1871 omitted
```

`agent stats`, which is an object rather than rows, renders as a two-column table of flattened paths
(`turns.durationMs.p95  9800`). The table is returned as a JSON string and the shell emits a string
result verbatim, so it prints as a table and not as a quoted scalar. Both formats carry
`truncated`/`omittedRows`, and a test asserts that for each.

## Bounds the provider enforces itself

`ExecutionConstraints` is `deny_unknown_fields` and a query-window key would be tree growth in
dekopon for a bound only this provider needs, so these live here:

- **`--since` at most 30 days**, hard-coded and stated in `--help`. The homelab store retains 30
  days; a longer window returns nothing anyway. Units are `s`, `m`, `h`, `d`; no fractions, no
  `1h30m`.
- **`--limit` at most 500.**
- **One statement, starting with `SELECT`** (or a `WITH` that ends in one). A `;` anywhere but the
  end is refused. Not a SQL parser — #250 rules one out — a gate against the two things that turn one
  authorized read into something else.
- **Results fitted under `--max-output-bytes`** (default 64 KiB), dropping rows from the tail and
  reporting `truncated: true` with an exact `omittedRows`. Rows are never dropped silently. The
  grant's own `maxOutputBytes` is the real ceiling and refuses rather than truncates; this is a
  courtesy, not a bound — a ceiling enforced by the thing being bounded is not a bound.
- **At most 200 sessions folded** into one `agent stats`, which is also the request-body budget: 200
  quoted trace ids is about 7 KiB of `IN (…)`, inside the 16 KiB `maxRequestBytes`.

## Two things #250 could not have known

Both were found reading the tree at `93246bb` and OpenObserve's own span record, and both changed
what the statements say.

**`accounting.model.turn` is not a queryable column.** `dekopond` wires only the tracer provider,
so a `tracing` event inside a span becomes an OTLP *span event*, and OpenObserve serializes a span's
events into one `events` **string** column (`Span { …, events: String, links: String }`). There is
no `audit_event` column for anything dekopond emits. The identical usage numbers are real span
attributes on the enclosing `prompt.model_turn` span — `usage.input_tokens` and its four siblings,
recorded by `record_usage` — and those fold to ordinary columns, so `agent stats` reads the span,
not the event. `dekopon-brokerd` *does* wire the logger provider since #217, so
`broker.decision`/`broker.execution` really are rows in the logs stream and the `broker` words read
them directly with `audit_event = '…'`.

**The agent id and the token counts are on different spans.** `agent` is on `gateway.session`; the
usage attributes are on the `prompt.model_turn` spans beneath it. They join by `trace_id`, which is
why `agent stats` plans its second statement *from the first answer* rather than emitting both at
once. #250 specified `plan(Word, Window) -> Vec<HttpRequest>`; the trait here is
`plan(&Query, prior: &[Response]) -> Option<Request>` for that reason. A subquery would have avoided
the round trip and bet the word on which DataFusion features the deployed store exposes.

## Result shapes

`agent stats`:

```json
{"agent":"reviewer","window":{"sinceUs":1788998400000000,"untilUs":1789084800000000},
 "sessions":2,
 "turns":{"count":41,"succeeded":41,"failed":0,"durationMs":{"p50":2140,"p95":9800}},
 "tokens":{"input":183220,"cachedInput":121004,"output":22410,"reasoningOutput":0,"total":205630},
 "capabilities":{"gh.pull-request.files":12,"gh.pull-request.read":17},
 "denials":{"policy-denied":1},
 "truncated":false}
```

`count` and every `tokens` figure are exact over every matching turn, computed in the store with
`count`/`sum`. The percentiles are DataFusion's `approx_percentile_cont`, which is what OpenObserve's
own service-graph queries use, and `duration` is a span's microseconds converted here to
milliseconds.

**`succeeded` and `failed` are not real numbers yet.** A turn's outcome lives on the
`accounting.model.turn` span *event*, which the paragraph above explains is not a column, and the
enclosing span's `span_status` is `Unset` for a successful turn and for most failures too. Rather
than emit a plausible-looking wrong number, `succeeded` is set to `count` and `failed` to zero.
Read them as "not measured". The honest fix is upstream: wire `optional_logger_provider` into
`dekopond` the way #217 did for `dekopon-brokerd`, and `accounting.model.turn` becomes a row with an
`outcome` column.

`openobserve sql`, `openobserve search`, `openobserve trace`, `broker usage`, `broker denials`:

```json
{"rows":[…],"returned":100,"total":1873,"truncated":true,"omittedRows":1773}
```

`broker providers`: `{"bootedAtUs":…,"providers":[…],"truncated":false}`, anchored on the last
`broker_started` inside the window and reading every `loaded broker provider` announcement after it.
The discriminator is `artifact_sha256 IS NOT NULL` rather than the record's message, because a log
body lands in a different column on different store versions while an attribute folds to a stable
one. `path` is in that record and deliberately not in the projection.

## Column folding

OpenObserve replaces every character outside letters, digits, and underscore in an attribute name.
`audit.event` is the column `audit_event`, `usage.input_tokens` is `usage_input_tokens`,
`decision.allowed` is `decision_allowed`. The `sql` schema says so where a model reads it, so a
statement written against the dotted names fails once and is then written correctly.

`stream-name=dekopon` routes both signals into `dekopon` streams; spans are **not** in the
`default` trace stream, and `type=` selects the signal. Stream `doc_num` stats lag, so count by
searching, never by stream stats.

## Credential

The component sets no `authorization` header, and a test asserts the only header it ever sets is
`content-type`. The credential is a DRN-bound Basic header injected inside the broker's native HTTP
engine, for destinations inside its binding, where no guest can observe it; the host rejects an
`authorization` header from a guest by construction rather than overwriting it.

It is a **dedicated OpenObserve user with the read-only role**, not the ingest token. The ingest
token belongs to a different user and never leaves `OTEL_EXPORTER_OTLP_HEADERS`. Creating a second
user is the whole isolation story: revoking search does not stop telemetry arriving, and a leaked
search credential cannot write a record. `examples/agent-telemetry/secret-map.yaml.example` is the
binding, one per capability id, pinned to `POST` on `segmentPrefix
/openobserve/api/default/_search` with `allowQuery: true` — the `?type=` is a query, and it is how
the store picks a stream.

## Plaintext

The OpenObserve API on the homelab is `http://rpi.lan/openobserve`, and the native HTTP host
refuses plaintext to any non-loopback destination ("plaintext HTTP is restricted to loopback
destinations"). In-cluster `http://openobserve.openobserve.svc:5080` is refused for the same reason.
So this provider makes zero calls against that deployment as it stands. Two ways out, in order of
preference:

1. **TLS on the `/openobserve` IngressRoute.** The h2c ingest route on 5081 stays as it is; only the
   5080 API route needs a certificate. Then `--url https://rpi.lan/openobserve` and nothing else
   changes.
2. **The broker-level plaintext allowlist**, merged as dekopon PR #252. The owner names the host in
   `broker.yaml` and opts the constraint set in:

   ```yaml
   http:
     plaintextHosts: [rpi.lan]
   constraintSets:
     openobserve.agent-stats:
       constraints:
         http:
           allowedHosts: ["rpi.lan:5080"]
           allowPlaintextLoopback: true
   ```

   Then `--url http://rpi.lan:5080/openobserve` reaches the store with the provider unchanged. It is
   an owner decision about a LAN they control, written where an auditor can see it — the right shape
   — and it is still a plaintext Basic credential crossing a home network, so TLS remains the better
   answer.

Either way the provider is unchanged: the endpoint is an argument, not a build constant.

## Errors

| Code | When |
|---|---|
| `invalid-input` | A window past 30d, a limit past 500, a statement that is not one read, a malformed URL or trace id, an unknown field. None costs a request. |
| `upstream-failure` | The store answered non-200, answered something that was not a search result, or could not be reached. |

An upstream refusal names the status and the thing an owner would change — 401 names the credential,
400 names the folded column spellings, `Denied` names `allowedHosts`, `HostCallLimit` names
`maxRequests`, `ResponseTooLarge` names `maxResponseBytes`. The store's own message is quoted once
to a 240-character ceiling; the broker's transport message is never echoed, because it can name a
resolved address and the error travels to a model.

## Test coverage, and what is not covered

**Pure Rust against recorded answers, which is everything this repository can prove.** 74 tests:
the window arithmetic and the cap, the projection exclusion list against every column any statement
names, the fitting and truncation counts, the SQL gate, the clap trees for all three words, both output formats and
the truncation marker in each, the exact bytes of every request (URI, `type=`, microsecond `start_time`/`end_time`, `size`, headers), and
the exact projection of every response from fixtures in
`crates/dekopon-openobserve-provider/tests/fixtures/`. The transport and the wall clock are both
injected at the same seam the component uses, so what the tests exercise is what ships.

**No component-level test.** `dekopon-provider-sdk-testkit` grants no HTTP — it builds its
authorization with `http: None` — so `invoke` cannot be exercised through it, and this provider's
every word is an `invoke` that makes at least one call. #250 records the fix as its own small PR
(give the testkit an `.http(hosts)` grant with a scripted transport; it is a test crate and grows no
capability), and until that lands the manifest, the command-word grammar, and the refusals are what
a component-level test would add — all of which are covered natively here.

**No live smoke.** The plaintext prerequisite above means the homelab store cannot be reached yet.
The fixtures are hand-built to OpenObserve's documented `_search` response shape and its own span
record (`Span { trace_id, span_id, flags, span_status, span_kind, operation_name, start_time,
end_time, duration, service_name, …, events: String, links: String }`), not captured from a live
call. The first live `openobserve sql --since 1h --limit 5` after TLS lands is the release smoke,
and it is not yet run.

## Building

```sh
./build.sh              # produces openobserve-provider.wasm and its .sha256
./scripts/validate.sh   # the shared shipping gate: local, CI, and release all run this
```

Requires Rust 1.98.1 and `wasm-tools 1.259.0` exactly; `build.sh` refuses anything else, because the
build is reproducible and a different compiler is a different artifact. The harness is a port of
dekopon's `examples/providers/build-component.sh`, salt included: a rustc proxy that normalizes
`-Cmetadata`, `--remap-path-prefix` for the source root, the cargo home, and the sysroot, and a
grep that fails the build if any local path survived into the component.

`0.1.0` is `476340` bytes, `sha256:34c004929dd109850c88d59a05d622823d60b8521cd3f3e16868fef64a2b41c3` on macOS arm64.

`0.2.0` is `477463` bytes, `sha256:cc61fb4244baa5344fe6ab07ebb0ee79181fe745ee72a797d63c6ca4782700dc` on macOS arm64.

The gate also asserts the things that stop being true quietly: the WIT mirrors match the pinned
crates byte for byte, the guest dependency tree contains no `wasi`, `wasm-bindgen`, or `js-sys`, no
hand-written `unsafe` exists in either crate, the core module imports exactly
`dekopon:http/client@1.0.0` and `dekopon:clock/wall@1.0.0` and nothing else, and the component
exports exactly `describe`, `invoke`, and `run-command`.

### The clock

`dekopon:clock/wall@1.0.0` is read once per `invoke`, because every statement carries an absolute
`start_time`/`end_time` in microseconds and a component has no clock of its own. Once, not per
step: the three statements of one `agent stats` must bound the same window, and a planner reading
the clock per step would widen it between them. It is readable during `invoke` only — `describe` and
`run-command` are pure by contract and the broker traps a component that reads it from either, which
is why a command word's proposal carries `sinceSeconds` and never a resolved window.

### The SDK pin

`= "0.15.0"` from crates.io for all three guest crates, never a branch: cargo resolves a `branch =`
dependency by fetching the ref, so deleting the branch upstream breaks every cold build, which is
exactly how the first out-of-tree provider rotted. CI reads the pin out of `Cargo.lock` and fetches
the WIT from tag `v0.15.0` to compare, so an interface change upstream fails loudly rather than
producing a component that mismatches the host it will be loaded into.

## Releases

Tag `v<major>.<minor>.<patch>`, annotated, on `main`. The release workflow re-runs the whole gate,
rebuilds the component from an independent checkout and `cmp`s the bytes, attests it with
`actions/attest-build-provenance`, publishes the GitHub release asset with its `.sha256`, and pushes
an identical single-layer OCI artifact to `ghcr.io/dekopon-agents/provider-openobserve`.

`gh attestation verify` prints nothing and exits 0 when stdout is not a TTY. Silence is not
evidence; re-run with `--format json` and read the subject digest and `buildSignerURI`.

## License

MIT OR Apache-2.0.
