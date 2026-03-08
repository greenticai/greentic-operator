# PR-07 — safe mode, degraded mode, and provider-health driven orchestration

## Goal
Add explicit runtime survival behavior while keeping the operator generic about provider implementations.

## Current-state note from PR-00 audit
Current readiness/health behavior is too narrow for this design:
- health is tied to telemetry/secrets readiness flags plus active-pack count in the inspected runtime path
- there is no generic provider-class health registry in operator core

This PR should replace that model with provider-class-driven health and enforcement, not layer semantics on top of the current implementation-specific flags.

## Deliverables

### 1. Generic provider health model
Track health for:
- session provider
- state provider
- telemetry provider
- required observer hooks if policy says required
- bundle source/resolver if relevant to control-plane operations

This model should become the source of truth for readiness/degraded decisions.

Health shape should be generic:
- status
- last_checked_at
- reason
- consecutive_failures
- recovery state

### 2. Safe mode and degraded mode
Support:
- `safe_mode: bool`
- `degraded_level: 0..3`

Suggested semantics:
- 0: normal
- 1: disable optional integrations / non-critical enrichments
- 2: block state-mutating execution paths that cannot be made safe
- 3: only allow static or administratively permitted minimal responses

These semantics should reference provider classes, not Redis/OTLP specifically.

### 3. Request-time enforcement
For each request path:
- determine required provider classes
- decide whether execution can continue
- emit reasoned outcome
- avoid implicit partial behavior

### 4. Audit guarantees
Emit required internal events on:
- enter safe mode
- leave safe mode
- degraded level changes
- provider outage
- provider recovery
- load shedding / refusal

## Tests
- provider outage simulations
- degraded transitions
- request classification tests
- readiness behavior tests
- audit coverage tests
