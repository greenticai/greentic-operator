# PR-05 — telemetry and observer integration through hooks and provider contracts

## Goal
Move telemetry and observer behavior out of core assumptions and into clean extensibility seams.

## Design direction
The operator should still emit structured runtime events, but:
- it should not hard-code one exporter pipeline as the only model
- it should not embed business-specific observer logic
- it should let telemetry providers and observer packs subscribe via contracts

## Current-state note from PR-00 audit
There is only a partial foundation for this work today.

Existing useful pieces:
- pre/post operation hook chains in `src/capabilities.rs` and `src/demo/runner_host.rs`
- offer parsing in `src/offers/registry.rs`
- ad hoc runtime/operator logging

Missing today:
- a stable internal event stream contract
- broad lifecycle observer phases
- explicit queueing/backpressure/isolation policy for observer pipelines

So this PR is partly extraction and partly new design, not just a clean wrapper around an already-general observer system.

## Deliverables

### 1. Internal runtime event model
Create a stable event envelope including:
- event_type
- ts
- tenant
- team
- session_id
- bundle_id
- pack_id
- optional flow_id / node_id
- correlation_id / trace id
- severity
- outcome
- reason code(s) where applicable

### 2. Telemetry provider seam
Define how a telemetry provider can receive:
- counters
- latency timings
- status transitions
- audit events
- optional trace/log envelopes

The operator should emit the same logical events regardless of backend.

### 3. Observer hook integration
Define hook phases and wiring for observer packs, for example:
- ingress received
- post-ingress routing
- flow start/finish
- node start/finish
- tool call outcome
- admin action
- bundle lifecycle change
- degraded/safe mode transition
- provider outage

Current code only has a narrow pre/post operation hook model. Any wider phase model here should be treated as a new contract, not assumed to already exist.

### 4. Mandatory operator audit guarantees
Even though observer handling is extensible, the operator must still guarantee that certain events are always emitted into the internal event stream:
- admin actions
- activation / rollback
- drain / resume / shutdown
- degraded/safe mode transitions
- critical provider failures

### 5. Backpressure and isolation
Observer and telemetry integrations must not be allowed to destabilize request execution.
Add:
- timeout bounds
- queue bounds
- drop/defer policy where appropriate
- clear failure accounting

## Tests
- event envelope schema tests
- telemetry seam tests with fake provider
- observer hook subscription tests
- backpressure/failure isolation tests
- mandatory audit coverage tests
