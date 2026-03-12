PR-OP-01
gtc op demo wizard — Bundle Composer using Existing Bundle APIs + Pack-Declared Setup
Goal

Add a generic wizard entrypoint:

gtc op demo wizard

that can create or update a bundle by:

selecting application packs + provider packs (from well-known catalog + custom OCI refs)

collecting pack-declared setup answers (no hard-coded provider fields)

creating tenants and optional teams

defining allow rules mapping tenants/teams → application packs

producing a deterministic plan first, then executing

Without defining a new bundle file layout in this PR.
Bundle creation must use existing Greentic internal bundle builder/materializer code (including CBOR artifacts and any required manifests).

Non-negotiables

No bundle structure in this PR doc.
Codex must discover the correct structure by auditing current code.

No hard-coded provider question fields.
Provider-specific questions come from each selected pack’s declared setup contract.

Determinism: planning and execution are separate. dry_run=true must be side-effect free.

Scope
In scope (MVP)

Create bundle with a user-specified name (not “demo” hard-coded)

Add packs:

well-known packs (catalog source)

arbitrary custom packs/extensions (OCI refs)

Run setup flows declared by packs to generate config artifacts

Tenant/team creation + allow model

Plan-first execution

Out of scope (MVP)

marketplace browsing UI beyond catalog list

infra provisioning / deployment

WASM warming

signature verification UX (may exist internally; use existing policy if present)

Work
0) Audit: how bundles are created today

Create docs/audit-bundle-creation.md covering:

Where bundle creation/materialization logic lives (crate/module paths)

What “bundle” means in operator terms (which data structures, manifests, CBOR artifacts, etc.)

How packs are added today (copy vs reference vs index)

How config/state is represented (CBOR/YAML/JSON)

Existing allow mechanism and resolver integration (trace `demo allow`)

What APIs are safe to call in a “plan-only” phase

Deliverable: a short map of the internal APIs the wizard must reuse.

Important: if there’s already a “bundle init” / “bundle write” helper, use it; do not reimplement.
Additional audit requirement: trace `gtc op demo start` and `gtc op demo allow` end-to-end and reuse their canonical update path so resolver-visible changes are immediate.

1) Add wizard provider module (plan-first)

Add src/wizard/ implementing deterministic API:

spec(mode, ctx) -> QaSpec

apply(mode, ctx, answers, dry_run=true) -> WizardPlan

Modes:

create (MVP)

update (optional stub; document as follow-up if too large)

Key principle

apply(..., dry_run=true) must produce a plan that describes operations but does not execute.

Execution should occur only in the CLI adapter after user confirmation.

2) Bundle operations must be expressed via internal APIs
Planning phase

In apply():

compute “desired bundle” outcome (bundle identity, selected packs, tenancy/allow model)

generate an ordered list of steps that call existing bundle APIs in execution mode

Execution phase

In CLI adapter:

execute plan steps using internal bundle writer/materializer

Codex instruction:
If internal APIs already do “create bundle with packs”, then a plan step should just call that API with resolved inputs. Avoid enumerating file-level write steps unless the internal API requires it.

3) Pack selection: catalog + custom refs

Wizard should support:

well-known pack listing (catalog provider; implementation detail decided by audit)

user-supplied pack locators (OCI refs / local refs if supported by existing resolver)

Do not bake GHCR paths or org assumptions into core logic.
Catalog implementation can default to greenticai-org sources, but it must be pluggable.

4) Pack-declared setup: ask what the pack declares

For each selected pack:

read pack metadata/descriptor to determine whether setup is declared:

has_setup

reference to qa spec / answers schema / apply answers op (whatever your codebase uses today)

Then:

run greentic-qa to collect answers for that pack (composed spec; no callbacks)

use pack’s own “apply answers” capability (or equivalent internal function) to produce config outputs

include those outputs as part of bundle creation/update plan

No provider-specific question IDs in operator.
Operator only asks generic bundle/packs/tenants/allow questions.

5) Tenants/teams and allow model (definitive: gmap + resolver pipeline)

The wizard must support:

creating tenants

optional teams per tenant

allow rules: map tenants/teams to app packs

Use existing gmap semantics (same behavior as `demo allow`):

Paths use `PACK[/FLOW[/NODE]]` syntax (max 3 segments)

Stored at:

`tenants/<tenant>/tenant.gmap`

or `tenants/<tenant>/teams/<team>/team.gmap`

Then the system must:

rerun resolver to produce `state/resolved/<tenant>[.<team>].yaml`

overwrite/copy `resolved/<tenant>[.<team>].yaml` so demo start sees updates immediately

Wizard must call/reuse the same underlying functions as `demo allow`; do not invent new allow storage.

6) CLI entrypoint

Add or update the command:

gtc op demo wizard

Behavior:

build top-level QaSpec (bundle name, pack selection, tenants/teams, allow)

for each selected pack with setup: run pack setup spec and collect answers

call provider apply(dry_run=true) to generate plan

show plan summary

confirm (or --execute)

execute plan using internal bundle APIs

Frontends:

preserve whatever frontends already exist (text/json/adaptive card)

do not reimplement renderers; reuse greentic-qa.

Tests
1) Plan snapshot

fixed answers produce stable plan JSON

no file IO during dry-run

2) Execution smoke test

execute plan into a temp directory

validate the result via existing “bundle validate/doctor” internal function (preferred)

if no validator exists, assert bundle can be loaded by the internal bundle loader

3) Pack setup integration smoke

use a fixture pack (or minimal test pack in repo fixtures) that declares setup

verify wizard runs setup and resulting bundle is valid

Docs

docs/wizard.md

how to run wizard

how catalog + custom packs works

how pack-declared setup is invoked

how tenants/teams/allow are modeled (conceptually)

docs/audit-bundle-creation.md

what internal APIs were used and why

Resolved design answers (locked)

1) Canonical bundle create/materialize/load/validate API

Use the internal APIs behind existing `gtc op demo` flows and bundle lifecycle that already maintain manifests, run resolver, and produce artifacts consumed by demo start. Bundle updates must integrate with resolver pipeline so changes are visible immediately (same behavior as `demo allow`).

2) Wizard CLI scaffold

Extend existing CLI and demo bundle subsystem. Preferred command: `gtc op demo wizard`. No new binaries or parallel command structures.

3) Pack resolver API

Use `greentic-distributor-client` for `oci://`, `store://`, `repo://`. Wizard must also trigger the canonical resolver rerun so demo start sees changes.

4) Well-known catalog source

Catalog returns refs only (friendly names/tags optional), does not fetch. Core wizard remains pluggable and avoids hardcoded GHCR/org assumptions.

5) Setup contract metadata representation

Audit existing descriptor formats (`has_setup`, `qa_spec_ref`, setup/apply ops), normalize legacy + canonical to an internal setup contract model, and ensure persisted outputs are resolver-visible.

6) Apply answers operation

Use canonical internal setup apply operation/helper (pack-declared op, e.g. `setup.apply_answers`). Do not implement custom provider setup logic in operator.

7) Setup outputs format

Treat outputs as opaque; persist only through internal bundle/demo APIs. Requirement: persisted outputs must be resolver-visible.

8) Tenant/team allow semantics

Use existing gmap allow model and flow:

write tenant/team gmap rules (`PACK[/FLOW[/NODE]]`, max 3)

rerun resolver

copy/overwrite resolved manifest so demo start sees updates immediately

9) No-allow fallback

N/A: allow mechanism exists and is authoritative (`gmap` + resolver pipeline).

10) WizardPlan granularity

Use high-level steps mirroring existing subsystems:

`ResolvePacks` (distributor-client)

`UpdateBundleManifest` (internal API)

`WriteGmapRules` / `ApplyAllowRules` (reuse demo allow path)

`RunResolver`

`CopyResolvedManifest`

`Validate`

11) Deterministic plan stability

Sort pack refs/tenants/teams/rules, keep stable step ordering, normalize paths, avoid temp absolute paths, and enforce stable gmap serialization order/newlines.

12) Confirmation UX / `--execute`

Default dry-run plan. Interactive prompt for execution. Non-interactive requires `--execute`. Never execute by default.

13) Post-exec correctness checks

Priority:

canonical resolver run (as `demo allow`)

confirm resolved manifest updated at `resolved/<tenant>[.<team>].yaml`

run doctor/validate when available

14) Update mode scope

Implement create now; keep update as follow-up unless trivial via existing APIs.

15) Backward compatibility

Wizard is additive and must preserve existing demo commands. `demo allow` semantics remain authoritative; reuse same underlying functions.

16) Patch policy for local overrides

Keep committed `[patch.crates-io]` override in `Cargo.toml` during implementation/test cycle. Add TODO/PR note to move to dev-only `.cargo/config.toml` before publish and remove from `Cargo.toml`.

17) `greentic-distributor-client` integration details

Plan carries refs (not downloaded files). Execution resolves/fetches via distributor client and uses its caching. Include returned digest/version metadata in plan metadata when available for determinism.

Acceptance criteria

greentic-operator wizard creates a bundle using existing internal bundle writer logic (including CBOR artifacts)

User can select well-known packs + custom packs/extensions

Wizard asks only generic operator questions + pack-declared setup questions

Tenants/teams + allow rules are captured and persisted into bundle using existing mechanisms

Plan-first, execution separate, dry-run side-effect free

Tests pass, and the bundle loads/validates via internal code

Codex prompt snippet (to embed at top of PR)

Implement as much as possible without repeatedly asking permission.
Do an audit first to locate the correct bundle creation/materialization APIs and pack resolver.
Do not invent a new bundle directory layout. Use existing internal bundle writer/loader.
Keep dry_run side-effect free and preserve existing CLI UX.
Use the existing gmap + resolver allow pipeline (same semantics as `gtc op demo allow`). Do not invent allow storage. Wizard must write gmap rules (`PACK[/FLOW[/NODE]]` up to 3 segments), rerun resolver, and ensure the resolved manifest is copied/overwritten so demo start sees changes immediately without rebuild. Leave committed `[patch.crates-io]` until test cycle is complete and publishing is ready.

