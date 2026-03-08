# PR-04 — integrate session and state provider contracts without hard-coding implementations

## Goal
Make the operator consume session/state functionality through provider contracts only.

## Important clarification
This PR is **not** "add Redis to the operator".

Instead it is:
- define what the operator needs from sessions and state
- route those needs through provider contracts
- keep the operator agnostic about whether the backing implementation is Redis, SQL, KV, memory grid, or something else

## Current-state note from PR-00 audit
The current problem is not direct Redis logic in the operator crate.

The actual current state is:
- operator/runtime behavior depends on session/state through lower-level runner-host adapters
- those adapters currently default to in-memory stores in the inspected runtime path
- the operator itself does not yet own an explicit provider-selection seam for session/state

So this PR must first move session/state ownership up to the operator boundary, then route implementation through provider contracts.

## Deliverables

### 1. Session contract integration
Define the minimum operations the operator requires, such as:
- get session context
- create/init session context
- update cursor/routing context
- attach bundle assignment
- compare-and-set or revision-aware update
- optional TTL/lease semantics
- delete/close session

The operator must not assume any specific storage medium.
It also must not rely on lower-level runtime defaults selecting a store implicitly.

### 2. State contract integration
Define minimum state operations for operator/orchestrated flows:
- get scoped state
- set/update scoped state
- merge or patch where supported
- delete scoped state
- optional transactional/compare-and-set hook
- optional TTL semantics

### 3. Routing and execution integration
All flow execution paths that need session or state must go through the provider seam.

That includes replacing any hidden dependence on runner-host storage adapters as the effective source of truth.

### 4. Health and dependency model
The operator should track provider health generically:
- available
- degraded
- unavailable
- unknown

without embedding Redis-specific metrics names in core.

### 5. Failure handling
Define generic behavior when required providers are unavailable:
- readiness impact
- retry/timeout envelope
- degraded mode transition
- error classification

## Tests
- fake provider integration tests
- session continuity across operator restart with mocked provider
- state mutation tests through seam only
- concurrency/revision tests
- degraded behavior when provider is unavailable
