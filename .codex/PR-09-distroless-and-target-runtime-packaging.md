# PR-09 — distroless packaging and target-runtime constraints

## Goal
Package the operator for production without inflating core responsibilities.

## Current-state note from PR-00 audit
Current runtime helpers assume a bundle-style local filesystem layout with writable `state/`, `logs/`, cache, and other local paths.

This PR therefore requires some path-policy redesign, not just container-image polishing.

## Deliverables

### 1. Distroless baseline
Provide:
- non-root runtime
- read-only root filesystem
- explicit writable dirs only
- no shell/package manager
- minimal startup surface

### 2. Runtime directory policy
Support only the minimum needed, for example:
- bundle cache/stage area
- mount point directory if mount mode is used
- temporary working directory
- optional node-local cache
- optional logs if not stdout-only

This policy must explicitly replace the current assumption that runtime state/log paths live under a writable local bundle root.

### 3. Access-mode-aware docs/examples
Document deployment expectations for:
- Kubernetes restricted profile
- Kubernetes privileged/mount-capable profile
- serverless container
- VM
- Snap
- Juju machine model
- Juju Kubernetes model

The docs must clearly state when mounted SquashFS is likely available and when userspace mode is expected.

### 4. Startup validation
Validate and report at startup:
- writable directory availability
- mount capability availability
- selected bundle access preference
- provider availability summary
- active policy summary

Validation should fail clearly when the runtime directory contract required by the new packaging model is not satisfied.

## Tests
- image smoke tests
- startup validation tests
- mount-preferred/fallback config tests
- docs/examples review checklist
