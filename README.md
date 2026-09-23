# dekopon-provider-openobserve

A bounded, read-only OpenObserve telemetry provider for Dekopon. The agent cannot choose the
endpoint, organization, stream, or SQL. The broker owner supplies one endpoint through
`providerSettings.openobserve` and grants individual capabilities with HTTP constraints and a
destination-bound search credential. This protects **agent-authored requests**; an OSS OpenObserve account
is not a stream-level RBAC boundary against a compromised provider or direct access to the store.

## Owner setup

This version needs a Dekopon broker with the `dekopon:settings/config@0.1.0` import. Settings are
available only during authorized `invoke`, never in `describe` or `run-command`:

```yaml
providerSettings:
  openobserve:
    url: https://openobserve-tls.openobserve.svc.cluster.local:5443/openobserve
    org: default
    stream: dekopon
```

These are nonsecret settings, not a capability grant. The owner must separately allow the exact
HTTPS authority, `POST` to `/openobserve/api/default/_search`, an appropriate request/response
budget, and a broker-injected `Basic` header bound to that destination (a legacy
`bearerToken` binding with `scheme: Basic` and a vault-supplied base64 `email:token`).
The native HTTP host
checks DNS addresses and pins resolution; if the address is non-public, the owner must separately
configure `http.nonPublicHttps`. Private CA trust is separately configured with
`http.extraCABundles`. Neither setting widens the capability's destination grant.

Only one owner-configured endpoint is supported. The provider validates the configured URL,
organization and stream before making a request and refuses any caller-supplied `url`, `org`, or
`stream`, including direct capability calls. HTTP transport errors are classified without echoing
host messages; upstream error bodies are not shown to the model.

## Capabilities and commands

| Capability | Command | Maximum `_search` calls |
|---|---|---:|
| `openobserve.trace` | `openobserve trace --since 1h <TRACE-ID>` | 1 |
| `openobserve.agent-stats` | `agent stats --since 1d --agent <ID>` | 3 |
| `openobserve.broker-providers` | `broker providers --since 1d` | 2 |
| `openobserve.broker-usage` | `broker usage --since 1h --by capability`, `broker denials --since 1h` | 1 |

All four return explicitly projected fields, not conversation text, span events, policy IDs,
credential names or paths. **There is no `openobserve.search`, `openobserve sql`, `search --where`,
or SQL-bearing input field.** Both command parsing and direct invocation reject attempts to supply
these. A future raw-SQL tool would require an explicit, wider authorization decision; attaching a
stream name to arbitrary SQL would not enforce a stream boundary.

All commands accept `--since` (1 second through 30 days), `--format json|table` and
`--max-output-bytes`; row-producing commands have `--limit` (1–500). JSON is the default and
contains `rows`, `returned`, `total`, `truncated` and `omittedRows`; `agent stats` returns a
statistics object. Command proposals carry only relative seconds. The broker's clock is read once
at invocation to form an absolute OpenObserve microsecond window. Results are fitted under the
provider output ceiling, while the broker's `maxOutputBytes` is the authoritative cap.

The `dekopon-otel-query-core` crate owns the bounded grammar and output format; the Wasm component
owns OpenObserve `_search` request planning. The provider never sets an Authorization header: the
native broker HTTP host injects the bound credential. Rows and upstream responses are untrusted;
this is a telemetry-summary capability, not a general SQL executor.

## Build and validation

```sh
cargo test --workspace
cargo fmt --all -- --check
../provider-workflows/build.sh
wasm-tools component wit openobserve-provider.wasm
```

The generated component must import `dekopon:http/client@1.1.0`,
`dekopon:clock/wall@1.0.0`, and `dekopon:settings/config@0.1.0`, and export `describe`,
`run-command`, and `invoke`. The broker runtime must be released before installing this provider;
older brokers cannot link the settings import. The shared provider CI/release workflow must accept
the project-owned `settings.wit` mirror before a release can pass its provenance gate. Artifacts
must be published and digest-pinned in the provider set, not loaded from a floating tag.
