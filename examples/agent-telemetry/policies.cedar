// policies.cedar — who may read which slice of the store. Nothing here says how narrowly the broker
// executes it: timeouts, hosts, methods, call budgets, and credential binding live in broker.yaml's
// constraint sets. Editing policy may broaden who can read; it cannot reach a new host.
//
// chmod 0600, owner-owned, single-link. Cedar validates this in strict mode at startup against a
// schema generated from broker.yaml's declared world, so a typo in a principal, provider, or
// capability refuses startup instead of becoming a rule that silently never matches.
//
// The split is the whole design. `openobserve.search` reads whatever a statement selects, including
// every conversation in the store, so it goes to the operator's own agent and nowhere else. The
// stats and fleet ids return projected numbers and can be granted more widely.

// The operator's own agent: the raw search, the trace fetch, and the fleet view.
@id("operator-may-search-the-store")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"openobserve.search",
       resource == Dekopon::Provider::"openobserve")
when { context has agent && context.agent == "telemetry-operator" };

@id("operator-may-fetch-a-trace")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"openobserve.trace",
       resource == Dekopon::Provider::"openobserve")
when { context has agent && context.agent == "telemetry-operator" };

@id("operator-may-read-the-fleet-view")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"openobserve.broker-usage",
       resource == Dekopon::Provider::"openobserve")
when { context has agent && context.agent == "telemetry-operator" };

@id("operator-may-list-loaded-providers")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"openobserve.broker-providers",
       resource == Dekopon::Provider::"openobserve")
when { context has agent && context.agent == "telemetry-operator" };

// Self-stats. `context.agent` pins the grant to the reviewer's own surface — and that is as far as
// scoping goes in 0.1.0. The provider takes `--agent <id>` as a flag, Cedar decides on
// `context.agent`, and it cannot compare provider input against context: the reviewer holding this
// grant can type `agent stats --agent librarian` and read the librarian's numbers.
//
// So this statement is honest only where every agent's numbers are fair game to every other, which
// is the case in a single-operator homelab and is not the case in a fleet with tenants. Until
// #248's `dekopon:meta/caller@0.1.0` import exists, a deployment that needs real per-agent scoping
// grants `openobserve.agent-stats` to the operator's agent alone and treats it as a fleet read.
@id("reviewer-may-read-its-own-stats")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"openobserve.agent-stats",
       resource == Dekopon::Provider::"openobserve")
when { context has agent && context.agent == "reviewer" };
