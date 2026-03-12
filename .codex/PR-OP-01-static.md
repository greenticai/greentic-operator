PR-OP-01 — Runtime static route hosting and WebChat hardcode removal
Title

Add generic runtime static-route mounting to greentic-operator and remove hard-coded WebChat/Direct Line routes

Goal

Make operator a generic runtime host for pack-declared static routes and provider-declared ingress routes, then remove the current hard-coded WebChat handling.

Why

Today operator hardcodes:

/token

/v3/directline/*

/directline/*

That needs to go.

Operator should instead:

own listener

own reserved runtime routes

read pack/provider metadata from active bundle

mount routes generically

serve assets generically

swap routes on activate/rollback/drain

Scope Part A — Generic static route runtime support
Read static-route extension during warm

During warm/discovery of active bundle packs, read:

greentic.static-routes.v1

Build a runtime static route plan from pack metadata.

Add runtime abstractions

Introduce small internal types such as:

StaticRouteDescriptor

StaticRoutePlan

ReservedRouteSet

ActiveRouteTable

Validate collisions

At warm time, reject conflicts with:

reserved control-plane endpoints

onboard endpoints

generic ingress prefixes kept by operator

duplicate static mount paths

ambiguous normalized paths

Serve files from active bundle

Use existing bundle access/read support to serve:

files under declared source_root

index_file

spa_fallback

content type / content length

basic cache behavior if present

Lifecycle swap

Mounted static route table follows:

warm validation

activate install

rollback restore

complete-drain cleanup

Scope Part B — Remove hard-coded WebChat routes
Remove operator special cases

Delete the hard-coded handling for:

/token

/v3/directline/*

/directline/*

Preserve reserved routes that are truly operator-owned

Keep only:

health/readiness/status

deploy/runtime/admin routes

onboard routes if still intentionally operator-owned

generic provider ingress framework

Result

After this PR, WebChat backend routes must come from provider metadata, not operator code.

Non-goals

no setup logic

no startup orchestration logic

no WebChat-specific GUI logic

no bundle-level summary requirement

Files likely touched

HTTP ingress dispatch

warm lifecycle discovery

runtime route table state

static file serving implementation

route collision logic

tests around activate/rollback/drain

removal of WebChat compatibility route branches

Acceptance criteria

operator mounts static routes from pack metadata

operator serves static assets for active bundle only

operator no longer hardcodes WebChat/Direct Line routes

warm fails cleanly on route conflicts

route table swaps cleanly across activation lifecycle