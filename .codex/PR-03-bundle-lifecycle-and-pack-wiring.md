# PR-03 — bundle lifecycle, capability discovery, and pack wiring

## Goal
Implement production-grade bundle lifecycle around immutable bundles, while wiring the active bundle into runtime capability discovery.

## Current-state note from PR-00 audit
This is largely net-new work.

Today there is:
- no first-class bundle registry
- no staged/warming/active/retired lifecycle object
- no atomic active-bundle swap model
- no structured warm report

There are only scattered archive-reading paths plus demo/runtime state helpers.

## Lifecycle states
Use:
- `Staged`
- `Warming`
- `Ready`
- `Active`
- `Draining`
- `Retired`

## Deliverables

### 1. Bundle registry
Track:
- staged bundles by id/digest
- active bundle pointer
- previous bundle pointer
- warm report
- active access mode (`mounted` / `userspace`)
- retirement/drain state

### 2. Capability discovery during warm
When warming a bundle, the operator must:
- read manifest and pack metadata
- discover capability offers
- discover hooks
- discover subscriptions
- validate duplicates/conflicts according to policy
- prepare a runtime wiring plan

Use the existing manifest/offer parsing logic as raw input only; do not preserve the current duplicated archive-reading paths as the final architecture.

### 3. Runtime wiring plan
Produce a structured plan showing:
- which pack provides session capability
- which pack provides state capability
- which pack provides telemetry capability
- which hooks run at ingress / post-ingress / flow / admin / error stages
- which subscriptions must be attached
- required vs optional runtime services

### 4. Activation
Activation must:
- require a successful warm
- swap active bundle atomically
- preserve previous pointer for rollback
- activate the wiring plan
- keep prior bundle available for draining sessions if policy requires

### 5. Rollback
Rollback must:
- re-activate the previous bundle and its wiring plan atomically
- emit lifecycle/audit events
- avoid partial provider state

## Tests
- warm-time capability discovery tests
- duplicate provider conflict tests
- activation/rollback tests
- drain behavior tests
- wiring-plan snapshot tests
