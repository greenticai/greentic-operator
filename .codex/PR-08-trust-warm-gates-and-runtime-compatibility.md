# PR-08 — trust policy, warm gates, and runtime compatibility validation

## Goal
Ensure the operator activates only bundles that are both trusted and runtime-compatible.

## Current-state note from PR-00 audit
Because there is currently no unified `BundleFs` or bundle access abstraction, compatibility validation in this PR must explicitly include the selected access mode and prove that the bundle can be used consistently through that mode.

## Deliverables

### 1. Warm checklist
Warm must validate:
- digest match
- media type correctness
- bundle integrity
- signature validity if policy requires it
- issuer trust if policy requires it
- pack metadata parseability
- capability/hook/subscription compatibility
- required provider capability presence
- duplicate/conflicting offers according to policy
- `BundleFs` usability in the selected access mode

### 2. Runtime compatibility report
Produce a structured warm report including:
- bundle id / digest
- selected access mode (`mounted` or `userspace`)
- capability discovery summary
- provider wiring summary
- warnings
- blocking failures
- trust decisions
- compatibility matrix

The report should make access-mode fallback explicit because that behavior does not exist in the current codebase and must be proven, not assumed.

### 3. Activation gate
Activation must refuse if:
- warm failed
- required provider capabilities are missing
- trust policy fails
- bundle cannot be opened consistently
- compatibility checks fail

### 4. Staged fallback on access mode
If mounted access fails during warm but userspace works:
- warm may still succeed
- report must clearly indicate fallback occurred
- activation should proceed only with the proven-good mode

## Tests
- invalid digest refusal
- media type mismatch refusal
- missing provider capability refusal
- duplicate-offer policy tests
- mount-fails-userspace-succeeds warm test
- successful warm/activate path
