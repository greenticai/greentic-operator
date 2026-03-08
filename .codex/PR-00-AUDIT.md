# PR-00 Audit — current `greentic-operator` removal/rewrite map for a minimal runtime core

## Scope
This audit is based on the current repository state on 2026-03-06. It covers:
- the `greentic-operator` crate itself
- the operator-facing runtime code in `src/demo/*`, `src/discovery.rs`, `src/capabilities.rs`, `src/offers/*`, `src/subscriptions_universal/*`, `src/runtime_state.rs`, `src/providers.rs`
- the runtime assumptions currently delegated into the vendored `greentic-runner-host` snapshot under `vendor/patches/greentic-runner-host/src/*`

It does not assume the vendored runner patch is wired via `[patch.crates-io]`; it is still useful as the closest local evidence of the host/runtime model this repo is coding against.

## Executive Summary
- The current crate is still primarily a local demo/dev orchestrator, not a production operator runtime.
- The strongest reusable foundations for later PRs are:
  - pack/capability discovery from `manifest.cbor`
  - capability/hook offer parsing and selection
  - provider op invocation through `DemoRunnerHost`
  - basic runtime-state persistence helpers
- The biggest blockers against the target architecture are:
  - runtime behavior is centered around `demo` CLI flows and local process supervision
  - session/state defaults are in-memory and storage semantics are buried below the operator seam
  - health/readiness are hard-coded to telemetry/secrets readiness instead of provider classes
  - admin behavior is a tiny reload/status surface with baked-in bearer-or-loopback auth
  - bundle access assumes `.gtpack` zip archives and extracted/materialized directories; there is no `BundleFs`, no SquashFS support, and no access-mode abstraction
  - subscriptions, onboarding, webhook setup, and ingress behavior are still messaging-specific in several places

## A. Module Map

### Keep as foundations

| Module | Purpose | Inbound callers | Outbound dependencies | Decision |
|---|---|---|---|---|
| `src/capabilities.rs` | Parses capability offers from `manifest.cbor`, resolves capability bindings, tracks install records | `src/demo/runner_host.rs`, doctor paths | `zip`, `greentic_types`, filesystem state under bundle root | Keep, but refactor into generic runtime registry pieces |
| `src/offers/registry.rs` | Parses pack offers for hooks/subscriptions/capabilities and selects by stage/contract | `src/demo/doctor.rs` and future discovery code | `zip`, `serde_cbor`, manifest structure assumptions | Keep, merge with future runtime capability registry |
| `src/discovery.rs` | Scans provider pack directories and extracts pack ids from `manifest.cbor` | demo startup/build/setup paths | filesystem layout, `zip`, `domains` | Refactor into bundle discovery layer |
| `src/runtime_state.rs` | Atomic JSON persistence and runtime path helpers | demo runtime, provider setup, subscriptions | local filesystem | Keep only the generic atomic write/read helpers; path model needs redesign |
| `src/provider_config_envelope.rs` | Stores provider config provenance/envelope beside runtime state | provider setup flows | `manifest.cbor`, local state dirs | Keep conceptually as compatibility/provenance helper, but move behind provider lifecycle service |
| `src/demo/runner_host.rs` | Closest thing to a runtime orchestrator: pack catalog, capability resolution, hook evaluation, provider invocation | HTTP ingress, subscriptions, setup QA, send/setup paths | `greentic-runner-host`, local state, local secrets, `CapabilityRegistry` | Refactor heavily; this is the main source of reusable runtime behavior |
| `src/subscriptions_universal/service.rs` | Demonstrates contract-driven provider operation invocation for subscriptions | scheduler/runtime paths | `DemoRunnerHost`, messaging DTOs | Keep as example of provider-driven orchestration, but generalize away from messaging DTO names |

### Refactor into runtime-core modules

| Module | Purpose | Inbound callers | Outbound dependencies | Decision |
|---|---|---|---|---|
| `src/domains/mod.rs` | Domain-specific provider directory mapping, setup/diagnostics/verify flow planning | CLI/demo setup/build/start | filesystem, pack manifests | Refactor; current shape is CLI/domain-specific, but some discovery helpers are reusable |
| `src/demo/http_ingress.rs` | HTTP ingress server and route dispatch for demo packs | `demo start` | `DemoRunnerHost`, messaging universal DTOs, onboard API | Refactor into generic ingress/data-plane surface |
| `src/subscriptions_universal/store.rs` | File-based subscription persistence | scheduler/runtime | local filesystem | Refactor behind generic state/session provider seams |
| `src/providers.rs` | Orchestrates setup/verify against provider packs | CLI setup flows | runner integration, secrets setup, provider config envelope | Refactor into provider lifecycle wiring; remove demo-specific assumptions |
| `src/services/*`, `src/supervisor.rs` | Local subprocess supervision for fake/demo services | `demo start/status/logs` | OS processes, pid files, logs | Refactor only if any process lifecycle helper is still useful; otherwise remove from core |
| `src/runner_integration.rs` | Shell wrapper around external runner CLI | provider setup, demo run/setup | subprocess execution | Treat as temporary compatibility shim only |
| `src/runner_exec.rs` | Direct runtime execution wrapper | `DemoRunnerHost` | `greentic-runner-host` and runtime config | Refactor or collapse into future runtime invocation service |
| `vendor/patches/greentic-runner-host/src/storage/*` | Session/state host adapters | runtime execution path | `greentic-session`, `greentic-state`, in-memory defaults | Refactor: useful evidence for seam shape, not acceptable as final ownership boundary |
| `vendor/patches/greentic-runner-host/src/http/{auth,admin,health}.rs` | Current admin auth, status, reload, health endpoints | HTTP server | `ServerState`, loopback/bearer auth, telemetry/secrets readiness | Refactor into production Admin API + policy hook seams |
| `vendor/patches/greentic-runner-host/src/pack.rs` | Pack loading/materialization/runtime composition | provider invocation | zip packs, dist fetch, tempdirs, Wasmtime | Refactor behind `BundleSource` + `BundleFs` abstractions |

### Remove from minimal runtime core

| Module | Purpose | Why it should not remain in core | Decision |
|---|---|---|---|
| `src/cli.rs`, `src/main.rs` | CLI/demo command tree | Current binary is a local tool shell, not the production runtime surface | Remove from runtime core; keep separately or shrink drastically |
| `src/demo/*` except logic migrated out | Demo bundle build/start/send/setup/new/logs/status tooling | Product-specific dev/demo orchestration | Remove from core after extracting reusable runtime pieces |
| `src/onboard/*` | Onboarding web/API/webhook convenience flow | Product/UI workflow, not core runtime ownership | Remove from core |
| `src/cloudflared.rs`, `src/ngrok.rs` | Local tunnel management | Vendor/dev infra behavior | Remove from core |
| `src/project/*`, `src/gmap/*`, `src/wizard*`, `src/setup_*` | Project scaffolding, policy files, interactive setup | Build-time or UX tooling | Remove from core |
| `src/messaging_universal/*` | Messaging-specific DTOs and ingress/egress behavior | Business/domain-specific logic | Remove from core; convert only required contracts into generic seams |
| `src/secrets_setup.rs`, `src/secrets_manager.rs`, `src/secrets_gate.rs`, `src/secrets_client.rs`, `src/secrets_backend.rs` | Local/dev secrets bootstrap and gatekeeping | Secrets are still needed by runtime, but these modules are strongly tied to dev/demo layout and local providers | Split: keep only minimal runtime secret contract integration, remove setup/dev parts |

## B. Runtime Ownership Matrix

| Responsibility | Current owner | Target owner | Notes |
|---|---|---|---|
| Sessions | Runner host storage adapters with in-memory default (`vendor/.../storage/session.rs`, `src/demo/runner_host.rs`) | Pack-provided via `SessionProvider` contract | Current operator does not own an explicit seam; storage medium is hidden below runtime API |
| Generic state | Runner host storage adapters with in-memory default (`vendor/.../storage/state.rs`) and file-based side stores in operator | Pack-provided via `StateProvider` contract | Multiple incompatible state stores exist today |
| Locks | No explicit generic lock seam found | Shared via contract or removed until required | Missing inventory item in current code |
| Rate limits | Present in runner host config types, not operator-owned as a generic seam | Shared via contract/policy | No operator-level provider-class abstraction yet |
| Telemetry export | Runner host telemetry module and readiness flag | Pack-provided via `TelemetryProvider` contract | Core should emit events, not own exporter pipeline |
| Audit emission | Ad hoc operator logs and some envelope emission in `DemoRunnerHost` | Operator-owned internal event stream, optionally exported by providers | Current code does not provide a stable audit/event contract |
| Observer event fanout | Pre/post hook chains and offer registry | Shared via contract | This is the closest existing seam, but it is still demo-centric and under-specified |
| Bundle cache | Runner host pack/cache internals and `.greentic/cache/provider-registry` | Operator-owned | Needs explicit bundle/cache ownership in core |
| Bundle registry | Not present as a first-class runtime service | Operator-owned | Must be added in later PRs |
| Readiness/health | Hard-coded telemetry/secrets readiness plus active-pack count | Operator-owned, driven by provider classes and bundle state | Current model is too narrow |
| Admin authz/authn hooks | Hard-coded bearer token or loopback allow in runner host | Shared via contract (`AdminAuthorizationHook`) | No hook seam exists today |

## C. Provider Seam Inventory

### Existing or partial seams

| Seam | Current evidence | Assessment |
|---|---|---|
| Session provider | `vendor/patches/greentic-runner-host/src/storage/session.rs` adapts `SessionStore` into runtime `SessionHost` | Partial. There is a storage contract underneath, but the operator does not select providers by capability/pack |
| State provider | `vendor/patches/greentic-runner-host/src/storage/state.rs` adapts `StateStore` into runtime `StateHost` | Partial. Same issue as session provider |
| Telemetry provider | `vendor/patches/greentic-runner-host/src/telemetry.rs` plus host helper telemetry interfaces | Partial. Telemetry exists as runtime behavior, but not as operator-selected provider packs |
| Observer hook subscription | `src/capabilities.rs`, `src/offers/registry.rs`, `src/demo/runner_host.rs` pre/post hook chain evaluation | Existing but underspecified. Best starting point for PR-05 |
| Bundle source / resolver | `src/provider_registry.rs`, runner-host pack/cache/dist code | Partial. There are fetch/cache behaviors, but no first-class `BundleSource`/`BundleResolver` seam |

### Missing seams

| Seam | Gap |
|---|---|
| `SessionProvider` | No operator-facing trait/registry selecting a provider pack for session lifecycle |
| `StateProvider` | No operator-facing trait/registry selecting a provider pack for state lifecycle |
| `TelemetryProvider` | No capability-driven provider selection or health model at operator level |
| `ObserverDispatcher` / `ObserverHookSink` | Hook invocation exists, but no stable internal event stream contract or isolation envelope |
| `AdminAuthorizationHook` | No policy/auth hook seam; auth is hard-coded |
| `BundleFs` | Completely missing |
| `BundleSource` / `BundleResolver` | No canonical abstraction separating source resolution from pack access |

## D. Bundle Runtime Inventory

### Current bundle formats
- `.gtpack` zip archives are the primary and effectively only supported pack/bundle format in operator code:
  - `src/discovery.rs`
  - `src/domains/mod.rs`
  - `src/provider_config_envelope.rs`
  - `src/component_qa_ops.rs`
  - `src/secrets_gate.rs`
  - `src/secret_requirements.rs`
  - `src/demo/runner_host.rs`
  - `src/offers/registry.rs`
- Materialized/extracted directories are also assumed in the runner host/runtime layer:
  - `vendor/patches/greentic-runner-host/src/pack.rs` `ComponentResolution.materialized_root`
  - cache/tempdir logic in runner host pack loading

### File access assumptions
- Zip archive random access via `ZipArchive` is hard-coded throughout operator-side discovery and metadata parsing.
- Several paths assume a bundle root with conventional subdirectories:
  - `providers/<domain>`
  - `packs`
  - `state/...`
  - `logs/...`
- Runtime state and install records are written relative to the bundle root.
- Some runner host paths assume local filesystem materialization before Wasmtime/component loading.

### SquashFS support status
- No local evidence of SquashFS support.
- No `sqsh`, `squashfs`, or mount-oriented bundle abstraction exists in `src/`.
- No OS mount lifecycle management exists in `src/`.
- No userspace SquashFS reader exists in `src/`.

### Code that assumes extracted directories or plain local files
- `src/runtime_state.rs` and many callers assume writable `state/` and `logs/` under a local root
- `src/services/runner.rs` assumes pid/log files on local disk
- `src/provider_registry.rs` caches registry files under `.greentic/cache/provider-registry`
- `vendor/patches/greentic-runner-host/src/pack.rs` normalizes pack paths, uses tempdirs, cache dirs, and filesystem-backed artifact resolution
- `vendor/patches/greentic-runner-host/src/lib.rs` writes `gtbind.index.json` under local paths config

## E. Deletion Candidates

### Old storage-specific modules to remove or isolate
- `vendor/patches/greentic-runner-host/src/storage/session.rs`
  - Keep only as a temporary compatibility shim if needed.
  - Current implementation defaults to `InMemorySessionStore`, which is incompatible with production runtime ownership.
- `vendor/patches/greentic-runner-host/src/storage/state.rs`
  - Same as above; defaults to `InMemoryStateStore`.
- `src/subscriptions_universal/store.rs`
  - File-based state store for one domain-specific feature; should not be operator core persistence.

### Telemetry-specific adapters to remove or move
- `vendor/patches/greentic-runner-host/src/telemetry.rs`
  - Keep event production concepts, move exporter-specific behavior behind a provider seam.
- `vendor/patches/greentic-runner-host/src/http/health.rs`
  - Current readiness is effectively telemetry/secrets readiness; this should be replaced, not extended.

### Obsolete lifecycle or demo-local orchestration
- `src/demo/runtime.rs`
- `src/services/*`
- `src/supervisor.rs`
- `src/cloudflared.rs`
- `src/ngrok.rs`
- `src/onboard/*`

These are useful for local demos, but they are not the minimal operator runtime described by PR-01+.

### Duplicate or fragmented bundle-loading paths
- `src/discovery.rs`
- `src/domains/mod.rs`
- `src/offers/registry.rs`
- `src/provider_config_envelope.rs`
- `src/component_qa_ops.rs`
- `src/secrets_gate.rs`
- `src/secret_requirements.rs`
- `src/demo/runner_host.rs`
- `vendor/patches/greentic-runner-host/src/pack.rs`

These all read pack archives directly. They should converge on a single bundle access layer.

### CLI/admin leftovers that no longer fit
- `src/cli.rs` top-level `demo` and `wizard` command tree
- `vendor/patches/greentic-runner-host/src/http/admin.rs` current `status` + `reload` surface
- `vendor/patches/greentic-runner-host/src/http/auth.rs` hard-coded auth policy

## Audit Answers to the PR-00 questions

### 1. Which modules currently hard-code Redis assumptions?
- No direct Redis usage was found in the operator crate itself.
- The closest evidence is the runner host feature flag `session-redis` in `vendor/patches/greentic-runner-host/Cargo.toml`.
- The actual code path inspected here defaults to in-memory session/state stores, not Redis.
- Conclusion: the current local problem is not Redis hard-coding in operator code; it is missing operator-owned provider seams and in-memory defaults hidden below the seam.

### 2. Which modules assume the operator itself is the source of truth for session or state?
- `src/runtime_state.rs` and callers use the local filesystem as truth for runtime/service state.
- `src/subscriptions_universal/store.rs` uses local JSON files as subscription truth.
- `src/provider_config_envelope.rs` uses local runtime files as source of truth for provider setup/config provenance.
- `vendor/patches/greentic-runner-host/src/storage/{session,state}.rs` hide backing stores behind host adapters selected by runtime construction rather than capability discovery.

### 3. Which observability paths are built directly into core and should become hook/provider seams?
- `vendor/patches/greentic-runner-host/src/telemetry.rs`
- `vendor/patches/greentic-runner-host/src/http/health.rs`
- operator logs in `src/operator_log.rs`
- envelope/hook emission in `src/demo/runner_host.rs`

### 4. Which startup/config paths assume one telemetry backend or one state backend?
- Health currently assumes telemetry + secrets readiness, not generic provider classes.
- `DemoRunnerHost::new` unconditionally constructs a single `new_state_store()` instance.
- The runner host storage adapters instantiate in-memory defaults through `new_session_store()` / `new_state_store()`.

### 5. Which code paths already resemble generic capability registration and can be retained?
- `src/capabilities.rs`
- `src/offers/registry.rs`
- `src/demo/runner_host.rs` capability resolution and hook evaluation

### 6. Where does bundle loading assume only extracted directories or only local files?
- Broadly across all zip-reading modules and all runtime-state modules listed in sections A and D.
- Most of the crate assumes a local writable bundle root plus local archive files.

### 7. What hook points already exist for provider packs, and are they generic enough?
- Pre/post operation hooks via `src/capabilities.rs` and `src/demo/runner_host.rs`
- Offer registry hook/subscription parsing in `src/offers/registry.rs`
- They are directionally useful, but not yet generic enough:
  - stages are narrow
  - event/audit envelope is local to demo runner execution
  - no explicit isolation, timeout, or backpressure contract

### 8. Which APIs expose implementation details instead of contracts?
- `src/providers.rs` exposes setup flows and runtime file layout directly
- `src/subscriptions_universal/service.rs` bakes messaging DTOs and provider op ids into operator code
- `vendor/patches/greentic-runner-host/src/http/auth.rs` exposes auth policy as implementation, not hook
- `vendor/patches/greentic-runner-host/src/http/admin.rs` exposes current runtime internals rather than a future control-plane contract

### 9. What code is dead, duplicated, legacy, or incompatible with the minimal-core direction?
- Most of `src/demo/*`
- process supervision in `src/services/*`
- tunnel/onboarding helpers
- duplicate archive parsing across many modules
- current runner host admin/health model

## Recommended PR sequencing impact

### PR-01 should start by extracting these reusable pieces
- capability/offer discovery logic from `src/capabilities.rs` and `src/offers/registry.rs`
- provider invocation orchestration concepts from `src/demo/runner_host.rs`
- atomic persistence helpers from `src/runtime_state.rs`

### PR-01 should explicitly avoid carrying these assumptions forward
- demo CLI ownership
- filesystem-as-truth for domain-specific state
- in-memory default session/state provider selection
- bearer/loopback admin auth
- telemetry/secrets-specific readiness
- zip-only direct archive reads scattered across the codebase

### PR-02 and PR-03 will require a genuine rewrite, not an incremental tweak
- there is no existing `BundleFs`
- there is no SquashFS path to preserve
- there is no first-class staged/warm/active bundle registry today

## Blockers and ambiguities
- `.codex/repo_overview_task.md` was referenced by repo instructions but not present in this checkout.
- The vendored `greentic-runner-host` snapshot is informative for architecture audit, but this crate does not currently patch `greentic-runner-host` to that local path. Follow-up implementation work should verify the exact upstream crate version in use before editing against vendored assumptions.
