# PR-02 — SquashFS runtime access: OS-mounted preferred, userspace universal fallback

## Goal
Implement a canonical bundle access layer for `.sqsh` bundles where:

- **mounted SquashFS via the OS is preferred when available**
- **userspace SquashFS reading is the fallback that always works**
- both paths expose the same `BundleFs` abstraction
- the rest of the operator is agnostic to how the bundle is accessed

## Why
Mounted SquashFS is preferable when supported because it gives:
- kernel filesystem integration
- page-cache benefits
- simpler directory-like access semantics
- efficient access for large assets

Userspace access remains essential because it works in:
- distroless containers
- restricted Kubernetes
- serverless containers
- Snap confinement
- Juju-managed deployments without mount privileges

## Deliverables

### 1. `BundleFs` abstraction
Expose a unified API for:
- reading `manifest.cbor`
- enumerating packs
- reading pack assets
- reading flows
- reading components
- reading shared assets/schemas/i18n
- opening by path
- metadata/stat-like queries where needed

### 2. Mount-capable bundle source
Implement a mount-aware path that:
- detects whether OS mount support is available and permitted
- mounts SquashFS in a controlled runtime directory when allowed
- records mount lifecycle for unmount/cleanup
- gracefully falls back when mount is unavailable or denied

### 3. Userspace reader
Implement a full userspace reader that supports the same `BundleFs` contract.

### 4. Selection policy
Default behavior:
1. if configured `prefer_mount = true`
2. and runtime capabilities permit mount
3. and mount succeeds
4. then use mounted access

Else:
- use userspace mode automatically

### 5. Operator visibility
Expose in status/diagnostics:
- bundle access mode: `mounted` or `userspace`
- mount attempt reason if fallback happened
- active bundle path/digest
- warm/cache status

## Safety requirements
- mount must never be required for correctness
- fallback must be automatic
- mount failures must not crash the process if userspace mode can proceed
- mount cleanup must be deterministic

## Tests
- mounted mode selection tests
- userspace fallback tests
- path parity tests between mounted and userspace
- diagnostics/status tests
- failure injection for mount denied / unsupported filesystem / cleanup failure
