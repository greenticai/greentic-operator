# PR-01 Post-Audit Implementation — Minimal Generic Operator Gap Fix (Only If Audit Requires It)

## Depends on

- `PR-00-audit.md`

## Goal

Implement only the smallest generic operator change necessary for a capability pack such as Greentic-X runtime to expose and run its operations cleanly.

## Important note

This PR should only proceed if the audit proves a real operator gap. If not, mark this PR as not needed.

## Possible scope areas to refine after audit

Examples only; replace with audited reality:

- generic capability operation registration gap
- operation discovery/lookup metadata gap
- runtime execution path bug
- hook/capability offer exposure gap
- event emission plumbing gap

## Constraints

- no Greentic-X-specific code
- no contract/runtime domain awareness
- no new product-specific config
- preserve operator minimalism

## Testing

Add or extend only the minimal tests needed to prove the generic gap is closed.

## Success criteria

A generic capability pack can expose and execute its operations through normal operator mechanisms without special cases.
