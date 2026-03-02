# Demo Wizard

## Command

`greentic-operator demo wizard`

## Behavior

- Builds a deterministic plan first.
- Default mode is dry-run (plan-only).
- Executes only with `--execute` and interactive confirmation.
- Reuses demo allow lifecycle semantics:
  - write gmap rules
  - rerun resolver
  - copy resolved manifest for demo start

## Inputs

- `--bundle <DIR>` target bundle path (must not exist on execute)
- `--mode <create|update|remove>` (default: `create`)
- `--catalog-pack <ID>` repeatable catalog ids
- `--catalog-file <PATH>` optional catalog file (JSON/YAML)
- `--pack-ref <REF>` repeatable custom refs (`oci://`, `repo://`, `store://`)
- `--tenant <TENANT>` tenant for allow rules (default: `demo`)
- `--team <TEAM>` optional team scope
- `--target <tenant[:team]>` repeatable tenant/team targets (overrides single tenant/team pair)
- `--allow <PACK[/FLOW[/NODE]]>` repeatable allow paths
- `--execute` execute instead of dry-run
- `--offline` resolve packs from cache only
- `--run-setup` run existing setup flows after execute
- `--setup-input <PATH>` optional setup answers passed to setup runner

## Catalog + custom refs

Catalog returns references only. Fetching happens in execution through distributor-client.

Catalog source options:
- built-in static list (default)
- custom file via `--catalog-file`
- custom file via `GREENTIC_OPERATOR_WIZARD_CATALOG`

## Pack resolution

Pack refs are resolved through `greentic-distributor-client` pack-fetch API.

- `oci://...` resolved directly.
- `repo://...` and `store://...` are mapped to OCI references using:
  - `GREENTIC_REPO_REGISTRY_BASE`
  - `GREENTIC_STORE_REGISTRY_BASE`

Fetched packs are copied into `bundle/packs/*.gtpack` with deterministic names.

## Setup invocation

Wizard plan includes `ApplyPackSetup` as a high-level step.

When `--run-setup` is enabled, wizard:
- builds a `FormSpec` for each selected pack via `qa_setup_wizard::run_qa_setup()`. The FormSpec is constructed from one of two sources (checked in order):
  1. A WASM component `qa-spec` op, if the provider supports it (converted by `qa_bridge::provider_qa_to_form_spec`).
  2. A legacy `assets/setup.yaml` inside the `.gtpack` (converted by `setup_to_formspec::setup_spec_to_form_spec`).
- collects answers per selected pack using the FormSpec-driven wizard. In interactive mode the wizard prompts for each question with type hints, secret masking, constraint validation (e.g. URL patterns), and choice enumeration. In non-interactive mode pre-supplied answers from `--setup-input` are validated against the FormSpec.
- validates all answers against the FormSpec (required fields, constraint patterns, choice membership).
- builds a preloaded answers map keyed by selected `pack_id`.
- invokes existing setup machinery (`run_domain_command` with `DomainAction::Setup`) for messaging/events/secrets domains, filtered to selected providers only.

After apply-answers completes, the wizard calls `qa_persist::persist_qa_results()` which:
- extracts secret fields (identified by `FormSpec.questions[].secret == true`) and writes them to the dev secrets store via `DevStore`.
- writes remaining non-secret fields to the provider config envelope, filtering out any secret values.

`remove` mode skips setup execution.

## Tenants/teams/allow model

Wizard uses existing tenant/team + gmap primitives only.

No new allow storage format is introduced.
