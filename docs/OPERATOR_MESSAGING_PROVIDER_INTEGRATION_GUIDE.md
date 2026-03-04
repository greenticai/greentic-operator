# Operator <-> Messaging Provider Integration Guide (Current)

Last updated: 2026-02-25

This document describes how `greentic-operator` currently integrates with messaging provider WASM components.

## 1. Runtime Architecture

`greentic-operator` does not invoke provider code directly. It uses `DemoRunnerHost`, which loads packs and delegates component execution to `greentic-runner-host` (`PackRuntime`).

Main path:

1. Discover provider packs (`.gtpack`) by domain.
2. Resolve provider binding from pack metadata (`provider_type`, component ref, world/export).
3. Load pack runtime with Wasmtime.
4. Invoke provider operation (`render_plan`, `encode`, `send_payload`, `ingest_http`, etc.).

Key files:

- `src/demo/runner_host.rs`
- `../greentic-runner/crates/greentic-runner-host/src/pack.rs`

## 2. Pack Discovery and Policy Filtering

Provider discovery is domain-based:

- Messaging: `providers/messaging`
- Events: `providers/events`
- Secrets: `providers/secrets`

For demo bundles (`greentic.demo.yaml` present), operator uses CBOR-only manifest discovery.

Tenant/team filtering is applied via resolved manifest (`resolved/<tenant>[.<team>].yaml`), not by raw filesystem listing only.

Key files:

- `src/domains/mod.rs`
- `src/cli.rs` (`resolve_demo_provider_pack`, `demo_provider_files`)

## 3. Host Capabilities Injected into WASM

At instantiation time, `PackRuntime` registers:

- WASI preview 2
- HTTP client host functions
- Secrets store host functions
- State store host functions (only when allowed by policy + component capabilities)

Current registration is done by `register_all(...)` in runner-host.

Important: state store is capability-gated per component manifest, not always available.

Key files:

- `../greentic-runner/crates/greentic-runner-host/src/pack.rs`

## 4. Outbound Flow (`demo send`)

Current `demo send` pipeline:

1. Build message envelope from CLI args.
2. Invoke provider `render_plan`.
3. Parse `RenderPlanOutV1`.
4. Invoke provider `encode` with the **actual `render_plan` output payload**.
5. Invoke provider `send_payload`.

This is the required contract for planned rendering flow.  
`encode` must consume the render output (`plan` object from step 2), not the original render input.

Key file:

- `src/cli.rs` (`impl DemoSendArgs::run`)

## 5. Inbound Flow (`demo ingress` and `demo start`)

There are two ingress shapes in operator:

### A) Universal messaging ingress (`demo ingress`)

Uses JSON DTO (`messaging_universal::dto::HttpInV1`) with:

- `body_b64`
- tuple headers `Vec<(String, String)>`
- tuple query `Vec<(String, String)>`

### B) HTTP gateway ingress (`demo start`)

Builds `IngressRequestV1` with:

- `body: Vec<u8>`
- tuple headers and tuple query
- then serializes as canonical CBOR before provider invocation

`ingress_dispatch` accepts provider response headers in both tuple/object forms and body in `body_b64`/`body`/`body_json`.

Key files:

- `src/messaging_universal/ingress.rs`
- `src/messaging_universal/dto.rs`
- `src/demo/http_ingress.rs`
- `src/demo/ingress_dispatch.rs`
- `src/demo/ingress_types.rs`

## 6. Provider Export Contract

Provider resolution uses pack metadata runtime binding (example: `schema-core-api` export on world `greentic:provider/schema-core@1.0.0`).

Runtime invocation path in runner-host:

- Resolve provider binding
- Detect schema-core world
- Call `schema-core-api.invoke(op, input-json-bytes)`

Key files:

- `../greentic-runner/crates/greentic-runner-host/src/pack.rs` (`invoke_provider`)
- provider pack manifests (for runtime binding)

## 7. Secrets Integration

Secrets are resolved via runner-host `SecretsStoreHost` using tenant context and pack/provider scope.
Operator demo path passes secrets manager handle into runtime load.

Key files:

- `src/demo/runner_host.rs`
- `../greentic-runner/crates/greentic-runner-host/src/pack.rs`

## 8. Error Handling

Provider op failure is propagated as unsuccessful `FlowOutcome` with enriched context.

Ingress dispatch:

- errors on provider invocation -> bad gateway path in HTTP server
- tolerant parsing for response headers/body formats

Send flow:

- parses node-error style JSON for retryability/backoff
- supports DLQ append for failed retries in end-to-end ingress path

Key files:

- `src/demo/runner_host.rs`
- `src/demo/http_ingress.rs`
- `src/messaging_universal/egress.rs`

## 9. Notes for Documentation Consumers

If you maintain external docs, align them with these implementation realities:

1. There are two ingress representations in operator (`HttpInV1` JSON + `IngressRequestV1` CBOR path).
2. Runtime import/export names are driven by generated bindings and runner-host registration, not legacy naming examples.
3. `demo send` contract is `render_plan -> encode(using render output) -> send_payload`.

## 10. Capability bootstrap checks in setup/start

`demo setup` and `demo start` now run a capability bootstrap report before provider flow execution.

Checks are scope-aware (env/tenant/team) and use Operator capability resolution:

- required (domain-driven):
  - `greentic.cap.messaging.provider.v1` for messaging domain
  - `greentic.cap.events.provider.v1` for events domain
  - `greentic.cap.secrets.store.v1` for secrets domain
- recommended (when messaging/events are enabled):
  - `greentic.cap.oauth.broker.v1`
  - `greentic.cap.mcp.exec.v1`

Operator also logs pending capability setup offers from `capability setup-plan` (`requires_setup=true` in `greentic.ext.capabilities.v1`).

This keeps setup/start aligned with capability-first orchestration and surfaces missing OAuth/MCP/Secrets capabilities early, without blocking existing flows by default.
