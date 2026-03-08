# PR-OP-02-wizard-operator.md

## Title
Refactor greentic-operator Wizard to Fully Delegate Q&A to greentic-qa (Concrete Create/Update/Remove Flows + Dynamic Providers + i18n)

---

# 🔴 Non-Negotiable Architectural Rule

greentic-operator **MUST NOT implement its own question/answer engine**.

All interactive handling, JSON-answer handling, branching, validation, and frontend rendering MUST be delegated to:

> ✅ greentic-qa (runtime engine)

greentic-operator is responsible ONLY for:

1. Building a concrete `QaSpec`
2. Calling greentic-qa runtime
3. Receiving validated answers
4. Building a deterministic `WizardPlan`
5. Executing or printing that plan

No manual prompting.
No local state machine.
No duplicated branching logic.

---

# High-Level Flow


CLI → Build QaSpec → greentic-qa.run() → Answers
→ build_plan(Answers) → WizardPlan
→ print summary (default)
→ execute if required


---

# CLI Contract

## Commands


greentic-operator wizard ...
greentic-operator demo wizard ...


`wizard` is a direct alias for `demo wizard`.

## Flags

| Flag | Meaning |
|------|--------|
| `--mode create|update|remove` | Skip main menu |
| `--bundle <path>` | Prefill bundle path |
| `--execute` | Force execute |
| `--dry-run` | Force plan only |
| `--locale <tag>` | i18n locale |
| `--verbose` | Print detailed plan steps |

CLI flags override answers.

---

# Provider Registry (Dynamic, OCI)

Operator loads:


GTC_PROVIDER_REGISTRY_REF
(default: oci://ghcr.io/greentic/registries/providers:latest)


Expected minimal structure:

```json
{
  "registry_version": "providers@1",
  "items": [
    {
      "id": "messaging.telegram",
      "label": { "i18n_key": "provider.telegram", "fallback": "Telegram" },
      "ref": "oci://ghcr.io/greentic/providers/messaging-telegram@0.6.0"
    }
  ]
}

These items generate provider selection questions dynamically.

No provider enums in source.

🔵 Concrete Wizard Flows

These flows are FIXED. Codex must not invent variations.

1️⃣ CREATE FLOW (Concrete Spec)
Step 1 – Bundle Basics

Questions:

bundle_path (required)

bundle_name (required)

locale (default en-GB)

Step 2 – App Packs (Repeatable Group)

Each pack entry includes:

pack_ref (required)

Accept:

local path

file://

oci://

store://

repo://

tenant_id (optional)

team_id (optional; requires tenant_id)

make_default

none

global

tenant (requires tenant_id)

team (requires tenant_id + team_id)

Validation rules:

team_id requires tenant_id

tenant/team default requires appropriate ids

Step 3 – Providers (Multi-select from OCI Registry)

Question:

providers[] (array of provider ids from registry)

No config questions in this PR.

Step 4 – Tenants & Teams

Question:

Repeatable:

tenant_id

teams[] (optional array of team_id)

Step 5 – Access Mode

Question:

access_mode:

all_selected_get_all_packs

per_pack_matrix

If matrix:
Repeatable:

pack_id

allow[]:

tenant_id

optional team_id

Step 6 – Execution Mode

execution_mode

dry_run

execute

CREATE → PLAN Mapping

Operator builds plan:

For each pack:

If oci/store/repo → resolve via greentic-distributor-client

Copy pack into:

bundle_root/packs/<packname>

If make_default:

global → bundle_root/default.gtpack

tenant → bundle_root/tenants/<tenant>/default.gtpack

team → bundle_root/tenants/<tenant>/teams/<team>/default.gtpack

For providers:

Register provider components in bundle config

For access:

Generate allow rules / gmap rules

Plan summary example:

Create bundle at ../tests/autobundle
Add 3 packs
Enable 2 providers
Create 2 tenants
Set 3 default packs
Add 5 access rules
2️⃣ UPDATE FLOW (Concrete Spec)
Step 1 – Bundle Path

bundle_path (must exist)

Step 2 – Select Operations (Multi-select)

update_ops[]:

packs_add

packs_remove

providers_add

providers_remove

tenants_add

tenants_remove

access_change

Step 3 – Conditional Subsections
packs_add

Same structure as CREATE pack entry.

packs_remove

Repeatable:

pack_identifier (file or ref)

optional scope:

global

tenant (requires tenant_id)

team (requires tenant_id + team_id)

providers_add

provider_id (from registry)

providers_remove

provider_id

tenants_add

Repeatable:

tenant_id

teams[]

tenants_remove

Repeatable:

tenant_id

optional team_id

access_change

Repeatable:

pack_id

operation: allow_add | allow_remove

tenant_id

optional team_id

Step 4 – Execution Mode

dry_run | execute

UPDATE → PLAN Mapping

Plan includes:

Fetch/copy new packs

Remove pack files or defaults

Modify tenant directories

Update provider registrations

Modify access rules

Always deterministic.

3️⃣ REMOVE FLOW (Concrete Spec)

Remove is destructive but scoped.

Step 1 – Bundle Path

bundle_path

Step 2 – Remove Targets

Multi-select:

packs

providers

tenants_teams

packs

Repeatable:

pack_identifier

optional scope (global/tenant/team)

providers

Repeatable:

provider_id

tenants_teams

Repeatable:

tenant_id

optional team_id

Step 3 – Execution Mode

dry_run | execute

REMOVE → PLAN Mapping

Plan includes:

Delete pack files

Remove default.gtpack files

Remove provider entries

Remove tenant/team directories

Clean up access rules

Plan Execution Rules

Always build plan first.

Default output: friendly summary.

--verbose: show step list.

If execute:

Execute steps sequentially

Stop on first error

Print partial success summary

i18n Architecture

Wizard strings defined in:

greentic-operator/i18n/operator_wizard/<locale>.json

Each QaSpec string:

{
  "i18n_key": "wizard.create.title",
  "fallback": "Create a new bundle"
}

Adding language:

Add new JSON file

No logic changes required

Locale selection:

--locale

fallback en-GB

Provider labels:

Use registry i18n_key

fallback to registry fallback

Modules to Implement
wizard_spec_builder.rs

Builds QaSpec based on:

mode

provider registry

CLI defaults

provider_registry.rs

Loads OCI registry.

wizard_plan_builder.rs

Answers → WizardPlan.

wizard_executor.rs

Executes plan steps.

No Q&A logic in these modules.

Acceptance Criteria

No manual prompting inside operator.

All questions rendered via greentic-qa.

Providers dynamically loaded from OCI.

Packs accept path/file/oci/store/repo.

default.gtpack rules respected.

Friendly summary default.

Verbose shows raw steps.

Alias greentic-operator wizard works.

Adding translation requires only adding JSON file.

Result

After this PR:

greentic-qa = Q&A runtime
greentic-operator = planner + executor
OCI registry = dynamic provider source
Wizard flows = deterministic and fixed

No invention.
No duplication.
Clean separation.
Scalable.

---

## Decisions for Open Questions

### 1) `--execute` and `--dry-run` both provided

- Behavior: CLI validation error.
- Exit code: `2`.
- Message: `--execute and --dry-run are mutually exclusive.`
- Rationale: avoid surprising precedence in automation.

### 2) Canonical `pack_identifier` for local paths / `file://`

Use a stable derived `pack_id`, persisted during create/add and reused for update/remove.

Rules:
- Local file/path: `pack_id = slug(pack_basename_without_ext)`.
- `oci|store|repo` refs: derive from last path segment + optional version tag (sanitized).
  - `oci://ghcr.io/greenticai/packs/sales@0.6.0` -> `sales-0_6_0`
  - `store://sales/lead-to-cash@latest` -> `lead-to-cash-latest`
- On collision: append `-2`, `-3`, etc.

Persist mapping under:
- `bundle_root/.greentic/packs.json` (or existing canonical metadata area)
- include: `pack_id`, `original_ref`, `local_path_in_bundle`, optional `digest`

Update/remove inputs:
- Prefer `pack_id`
- Allow free-text `pack_ref` as fallback

### 3) `packs_remove` scope omitted

Default scope: bundle-level pack file only.

- Remove `bundle_root/packs/<pack_id>.*` (or directory).
- Do not remove `default.gtpack` unless explicitly targeted.
- If a default points to removed pack:
  - mark as dangling
  - emit warning in summary
- Optional cleanup step allowed: `UnsetDefaultIfDangling`, but it must be explicit in plan output.

If user wants defaults removed, they must specify scope: `global|tenant|team`.

### 4) `tenants_remove` behavior

Removing a tenant removes everything under that tenant:
- all teams
- tenant default: `tenants/<tenant>/default.gtpack`
- team defaults: `tenants/<tenant>/teams/*/default.gtpack`
- tenant/team access rules for that tenant

Removing a team removes:
- that team directory
- that team default
- access rules for that team only

Execution must print a clear deterministic deletion summary.

### 5) `access_change` target format (`pack_id`)

Canonical key is logical `pack_id`.

Answer shape:
- `pack_id` required

Resolution:
- operator resolves `pack_id` via `packs.json`

Expert JSON mode:
- may allow `pack_ref` instead of `pack_id`
- plan builder must normalize to `pack_id` early

### 6) Provider registry fetch/auth + offline mode

Credential source order:
1. Existing operator config/secrets OCI pull mechanism
2. Standard OCI env/tooling (`DOCKER_CONFIG`, helpers), if supported by existing resolver
3. Anonymous pull attempt

Offline mode:
- Add/keep `--offline` support.
- If offline or network unavailable:
  - use cached registry if present
  - if no cache: hard fail with actionable message:
    - `Provider registry unavailable and no cached copy found. Re-run without --offline or set GTC_PROVIDER_REGISTRY_REF to a local file.`

This preserves deterministic provider-source guarantees.

### 7) Registry failure behavior in normal mode

Default behavior: hard fail.

Error should include:
- registry ref
- root cause
- hint to use `--provider-registry file://...` local override

Optional extension (not default):
- `--allow-missing-registry` to continue with empty provider options

### 8) Update/remove idempotency guarantees

Required behavior:
- idempotent no-op success where safe

Examples:
- remove already-removed item -> plan emits `NoOp`, execute succeeds
- add already-present pack/provider/tenant/team -> no-op (unless future `--force`)

Summary must report no-op counts explicitly.

### 9) Bundle config schema for provider registration

Do not invent arbitrary file formats when canonical manifest exists.

Default (if no canonical existing location):
- `bundle_root/providers/providers.json`

Structure:

```json
{
  "providers": [
    { "id": "messaging.telegram", "ref": "oci://...", "enabled": true }
  ]
}
```

Merge strategy:
- load existing file if present
- upsert by `id`
- preserve unknown fields if feasible

If repo already has canonical provider-manifest location, use it with same upsert semantics.

### 10) Wizard alias help text + command tree ordering

- `greentic-operator wizard` and `greentic-operator demo wizard` are equal peers in help.
- Document `wizard` as preferred primary.
- Keep `demo wizard` for compatibility.
- Help text for `demo wizard` should say it is an alias of `wizard`.

---

## Additional Explicit Tasks (Authoritative)

- CLI validation:
  - enforce mutual exclusion for `--execute` and `--dry-run`
  - add/keep `--offline`
  - add `--provider-registry <ref>` override flag (env/config default fallback)
- Bundle metadata:
  - implement `packs.json` mapping store
  - normalize all pack references to `pack_id` at plan-build time
- Registry caching:
  - cache by digest/tag resolution
  - fail loudly with actionable error when cache is missing
- Idempotency:
  - plan builder emits explicit `NoOp` steps where applicable
  - executor treats `NoOp` as success
