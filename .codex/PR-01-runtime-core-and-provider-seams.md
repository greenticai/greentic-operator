# PR-01 — refactor operator into minimal runtime core with explicit provider seams

## Goal
Refactor the operator into a small, stable runtime core whose primary job is to:
- load and activate bundles
- discover packs and their contracts
- wire capabilities/hooks/subscriptions
- execute ingress and flow orchestration
- expose admin and lifecycle controls

This PR must remove or isolate assumptions that storage, sessions, telemetry, and observers are implemented in core.

## Current-state note from PR-00 audit
The current repo is still heavily centered on local demo/dev orchestration.

The main reusable runtime foundations already present are:
- capability offer parsing in `src/capabilities.rs`
- hook/subscription/capability offer parsing in `src/offers/registry.rs`
- provider invocation and hook-chain orchestration in `src/demo/runner_host.rs`

This PR should extract and normalize those pieces into runtime-core modules.

This PR should **not** try to preserve large parts of:
- `src/demo/*` as-is
- `src/services/*`
- CLI/project/wizard scaffolding
- onboarding/tunnel helpers

Those are not the target runtime core.

## Key principle
The operator should know the **minimum** necessary to let the platform be maximally extendable.

## Deliverables

### 1. Introduce explicit runtime seam interfaces
Add operator-facing interfaces/contracts for:

- `SessionProvider`
- `StateProvider`
- `TelemetryProvider`
- `ObserverDispatcher` or `ObserverHookSink`
- `AdminAuthorizationHook`
- `BundleSource` / `BundleResolver`
- `BundleFs`

These are runtime abstractions. The operator depends on them by contract, not by implementation.

### 2. Add provider discovery/wiring model
The operator must be able to discover from the active bundle:
- which pack offers session provider capability
- which pack offers state provider capability
- which pack offers telemetry capability
- which packs subscribe to observer hooks
- which hooks must be invoked at which stages

Discovery should be capability/contract-driven, not special-cased by pack name.

### 3. Define clear ownership
The operator owns:
- lifecycle of provider instances
- invocation ordering
- backpressure, timeout, and failure envelopes
- wiring of provider outputs into execution paths

The provider packs own:
- actual session persistence implementation
- actual state storage implementation
- actual telemetry backend emission
- actual observer-specific transformation/sink logic

### 4. Remove hard-coded implementations where possible
For any existing embedded storage/session/telemetry behavior in operator:
- either remove it
- or move it behind the seam as a temporary adapter
- mark temporary adapters clearly as compatibility shims

Important current-state clarification:
- the operator crate itself does not currently hard-code Redis directly
- the more immediate problem is that storage/session/state ownership is hidden below the operator seam and defaults to in-memory behavior in the current runtime stack
- PR-01 should therefore focus first on introducing operator-owned seams and registry-driven wiring, not on a Redis-specific cleanup

### 5. Add runtime capability registry
Implement a registry that records:
- capability id
- contract id
- provider pack
- entrypoint
- scope
- lifecycle state
- health

This registry becomes the foundation for later PRs.

## Non-goals
- Do not implement a specific Redis provider here.
- Do not implement a specific OTLP provider here.
- Do not implement a specific observer sink here.

## Tests
- capability registry tests
- provider selection tests
- failure isolation tests
- startup wiring tests
- tests proving operator can start with interfaces mocked/faked

## Wizard/process guidance
Where scaffolding or fixtures are needed:
- use `greentic-pack wizard` to create/update test packs
- use `greentic-flow wizard` to create/update flows for runtime wiring tests
- use `greentic-component wizard` to create provider components if needed
- use replayed answers committed as fixtures for deterministic CI
