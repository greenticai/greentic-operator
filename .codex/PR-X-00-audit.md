# PR-00 Audit — greentic-operator Capability Runtime Exposure Audit for Greentic-X

## Goal

Audit `greentic-operator` to determine whether any changes are actually needed to host a Greentic-X runtime capability pack, or whether the current operator model already supports this with zero or near-zero changes.

This audit is mandatory before any implementation PR in this repo.

## Why this audit exists

The intended architecture is that:

- Greentic-X runtime is just another capability pack
- operator stays minimal and neutral
- operator should not learn contract semantics
- if the current capability loading/execution/exposure model already works, we should not modify operator unnecessarily

This audit prevents speculative runtime changes.

## Audit scope

### 1. Capability pack loading and execution
Inspect the real code paths for:
- pack discovery/loading
- component execution
- operation exposure
- capability offers / capability discovery / hook offers
- setup/default/update/remove lifecycle handling

Document exactly how a pack exposes executable operations today.

### 2. Public/internal operation surface
Determine how operations become callable:
- direct runtime execution
- routing through pack/component/operation references
- provider/capability registration
- hook chains

Establish whether a Greentic-X runtime pack can expose operations such as:
- `contract.install`
- `resource.create`
- `op.call`

purely through current patterns.

### 3. State, sessions, telemetry, observers
Audit how operator delegates:
- state/session concerns
- telemetry/audit concerns
- observer/hook concerns

Confirm that Greentic-X runtime can rely on these existing providers instead of duplicating infrastructure.

### 4. Event emission/subscription model
Check whether capability/app events emitted by a runtime pack can already flow through existing event or observer mechanisms.

### 5. Gaps, if any
If operator is missing anything, identify the smallest generic gap, for example:
- capability operation discovery metadata
- generic runtime API exposure shape
- no-op routing/registration bug
- lifecycle/init gap

## Deliverables

This audit PR must produce:

1. `docs/audits/greentic-x-operator-audit.md`
2. an explicit recommendation:
   - **Option A:** no code changes needed
   - **Option B:** tiny generic operator changes needed
3. updated follow-up PR(s) in this folder with exact real code locations, only if needed

## Constraints

- Do not add contract-aware logic
- Do not add domain models
- Do not special-case Greentic-X
- Prefer zero change unless the audit proves a real gap

## Exit criteria

A human can say with confidence whether Greentic-X runtime can be deployed as a normal capability pack on current operator infrastructure.
