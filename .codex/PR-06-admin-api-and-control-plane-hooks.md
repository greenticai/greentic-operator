# PR-06 — production Admin API with control-plane hooks

## Goal
Provide a production Admin API that manages operator lifecycle without baking policy logic directly into core.

## Current-state note from PR-00 audit
This is a replacement of the current model, not a minor extension.

The currently inspected runtime/admin behavior is roughly:
- a small `status` and `reload` surface
- hard-coded admin auth policy based on bearer token or loopback access
- health/readiness tied to narrow implementation-specific checks

This PR should explicitly replace that with the production control-plane contract.

## Endpoints
Implement at minimum:
- `/healthz`
- `/readyz`
- `/status`
- `/runtime/drain`
- `/runtime/resume`
- `/runtime/shutdown`
- `/deployments/stage`
- `/deployments/warm`
- `/deployments/activate`
- `/deployments/rollback`
- `/config/publish`
- `/cache/invalidate`
- `/observability/log-level` (optional if supported cleanly)

## Control-plane policy seam
Authentication/authorization should be hookable:
- mTLS identity extraction in core
- authorization decision delegated through an admin auth hook/policy seam
- all actions still audited by core

Do not preserve the current bearer-or-loopback model as the long-term control-plane policy shape except as a temporary compatibility shim if strictly necessary.

## Bundle operations
Admin bundle operations should drive:
- stage from a resolved/pinned bundle ref
- warm using the current warm policy
- activate atomically
- rollback atomically

Core must orchestrate; it must not assume where policy/config comes from beyond the contract.

## HA behavior
For mutating operations:
- either enforce leader-only mutation
- or return a structured `not_leader` response
- document the exact expected behavior

## Status response
Return a status object that includes:
- node id
- start time
- ready/draining state
- active bundle id
- active bundle access mode
- staged bundles
- discovered providers
- provider health summary
- current degraded/safe mode

## Tests
- endpoint contract tests
- auth hook tests
- audit emission tests
- leader/non-leader mutation tests
- status payload tests
