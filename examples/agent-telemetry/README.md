# agent-telemetry

One broker, one operator, one OpenObserve store. Everything an owner has to write to let a model
read its own telemetry, in three files.

```
broker.yaml                 constraint sets, provider set, identities
secret-map.yaml.example     the DRN-bound read-only OpenObserve user
policies.cedar              who may read which slice
```

Run it:

```sh
cp secret-map.yaml.example secret-map.yaml
chmod 600 secret-map.yaml broker.yaml policies.cedar
(cd ../.. && ./build.sh)          # produces ../../openobserve-provider.wasm
dekopon-brokerd --config broker.yaml
```

Then, from a shell session the broker grants:

```sh
agent stats --url https://rpi.lan/openobserve --agent reviewer --since 24h
broker usage --url https://rpi.lan/openobserve --since 24h --by capability
openobserve trace --url https://rpi.lan/openobserve --since 1h 0af7651916cd43dd8448eb211c80319c
openobserve sql --url https://rpi.lan/openobserve --since 1h --limit 5 \
  "SELECT operation_name, duration FROM \"dekopon\" WHERE service_name = 'dekopond' ORDER BY _timestamp DESC LIMIT 5"
```

`--url` is `https://` here, not `http://`. The store is `http://rpi.lan/openobserve` today and the
native HTTP host refuses plaintext to any non-loopback destination; the repository README's
**Plaintext** section is the whole story and both ways out of it.

## What each file is load-bearing for

**`broker.yaml`** — `maxRequests` per capability, because the plan differs per word. `agent stats`
is three `_search` calls: the agent's sessions, the model turns beneath them joined by `trace_id`,
then its broker decisions. The first two are `type=traces` and the third is `type=logs`, so they
cannot collapse into one statement. `broker providers` is two: the last `broker_started` in the
window, then everything announced after it.

**`secret-map.yaml`** — a *second* OpenObserve user, created for this and given the read-only role.
The ingest token is a different user and never leaves `OTEL_EXPORTER_OTLP_HEADERS`. One binding per
capability id, each pinned to `POST` on `segmentPrefix /openobserve/api/default/_search` with
`allowQuery: true` — the `?type=traces|logs` is a query, and it is how the store picks a stream.
`maxInjections` may not exceed the constraint set's `maxRequests`.

**`policies.cedar`** — `openobserve.search` to the operator's own agent and nobody else, because it
reads every conversation in the store. Everything else returns a projection with no conversation
text, no principal, no policy id, no credential name, and no path.

The one dishonest-looking statement is the last, and the comment above it says why: 0.1.0 takes
`--agent <id>` as a flag, Cedar decides on `context.agent`, and it cannot compare provider input
against context. An agent holding `openobserve.agent-stats` can name any agent. That is fine in a
single-operator homelab and wrong in a fleet with tenants; until dekopon grows the
`dekopon:meta/caller@0.1.0` import (#248), a multi-tenant deployment grants this id to the
operator's agent alone and treats it as a fleet read.
