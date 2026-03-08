# greentic-operator: Stage 1 Audit (PR-OP-05)

Purpose: factual snapshot of current wizard implementation for surgical Stage 2 changes.

Date: 2026-03-02

## Scope covered
- Wizard entrypoints (CLI aliases, command path, modules)
- Current schema/question representation and versioning state
- Execution model (plan vs execute and side-effects)
- i18n handling + locale resolution
- Existing non-interactive answer support
- Delegation to other execution paths/binaries
- Test coverage on wizard paths

## Environment note
- `cargo run -- wizard --help` and `cargo run -- demo wizard --help` could not be executed in this environment because crates could not be downloaded (`Could not resolve host: static.crates.io`).
- Therefore CLI help is captured from clap source (`#[command(... after_help = "...")]`) and argument annotations.

## A) CLI surface
- [x] List `wizard` subcommands and flags
- [x] Note locale/answers flags and equivalents
- [x] Note default interactivity and mode selection

### Source-based help text (exact `after_help` content)
From `src/cli.rs:476`:

```text
Main options:
  --mode <create|update|remove>
  --bundle <DIR> (or provide in --qa-answers)

Optional options:
  --qa-answers <PATH>
  --catalog-pack <ID> (repeatable)
  --pack-ref <REF> (repeatable, oci://|repo://|store://)
  --provider-registry <REF>
  --locale <TAG> (default: detected from system locale)
  --tenant <TENANT> (default: demo)
  --team <TEAM>
  --target <tenant[:team]> (repeatable)
  --allow <PACK[/FLOW[/NODE]]> (repeatable)
  --execute
  --dry-run
  --offline
  --verbose
  --run-setup
```

### Findings
| Item | Current behavior | Files/lines |
|---|---|---|
| Wizard command path(s) | Two CLI entrypoints: top-level `wizard` alias and `demo wizard` subcommand; both call `DemoWizardArgs::run()` | `src/cli.rs:85-92`, `src/cli.rs:102-134`, `src/cli.rs:1724-1727`, `src/cli.rs:1764`, `src/main.rs:136-140`, `src/main.rs:294-299` |
| Flags for locale | Global `--locale` on root CLI plus wizard-local `--locale` for QA rendering. Effective locale for wizard QA defaults to detected system locale when wizard-local locale is absent. | `src/cli.rs:79-83`, `src/cli.rs:533-534`, `src/cli.rs:2487`, `src/main.rs:15-16`, `src/main.rs:44-64` |
| Flags for answers import/export | No `--answers` / `--emit-answers`. Equivalent today: `--qa-answers <PATH>` imports JSON/YAML answers; dry-run path prompts for an output filename and writes raw QA answer JSON. | `src/cli.rs:484-487`, `src/cli.rs:2517-2520`, `src/cli.rs:2897-2908`, `src/cli.rs:2707-2718`, `src/cli.rs:2841-2861` |
| Validate/apply split | No separate validate/apply subcommands. Single wizard flow always builds plan; if not executing, it saves answers and exits. Execution happens only when `execute_requested` is true. | `src/cli.rs:2671-2672`, `src/cli.rs:2707-2719`, `src/cli.rs:2745`, `src/wizard_plan_builder.rs:7-9`, `src/wizard_executor.rs:4-8` |
| Non-zero exit handling | Clap parse errors use clap exits; missing subcommand exits `2`; runtime errors bubble via `anyhow` (non-zero). User cancel on overwrite is treated as success (`Ok(())`). | `src/main.rs:26-39`, `src/main.rs:34-37`, `src/cli.rs:2731-2739` |

Default interactivity behavior:
- If `--qa-answers` is provided: non-interactive load path.
- Else wizard uses QA runner; interactive mode requires both stdin and stdout TTY; otherwise non-interactive QA run is attempted and may fail with “NeedsInteraction”.
- Default operation is plan-only unless `--execute` is set or answers include `execution_mode = execute`.
Refs: `src/cli.rs:2517-2527`, `src/cli.rs:2930-3003`, `src/cli.rs:2660-2670`.

## B) Schema + questions
- [x] Locate question definitions
- [x] Determine schema identity/versioning state
- [x] Confirm i18n key system

### Findings
| Item | Current approach | Files/lines |
|---|---|---|
| Schema identity | Data-driven JSON schema-like documents per mode with IDs: `operator.wizard.create`, `operator.wizard.update`, `operator.wizard.remove` | `src/wizard_spec_builder.rs:58`, `src/wizard_spec_builder.rs:142`, `src/wizard_spec_builder.rs:288` |
| Schema versioning | Per-mode `version: "1.0.0"` exists inside validation form. No top-level AnswerDocument envelope fields (`wizard_id`, `schema_id`, `schema_version`) in persisted wizard answers. | `src/wizard_spec_builder.rs:60`, `src/wizard_spec_builder.rs:144`, `src/wizard_spec_builder.rs:290`, `src/cli.rs:2709-2712`, `src/cli.rs:2906-2908`, `src/cli.rs:550-582` |
| Question model | Questions are defined as JSON in `wizard_spec_builder` (types `string|enum|list`, required, choices, nested list fields). There is also an older lightweight Rust `QaSpec/QaQuestion` struct path in `wizard.rs`, but runtime QA uses `wizard_spec_builder` JSON. | `src/wizard_spec_builder.rs:45-126`, `src/wizard_spec_builder.rs:129-283`, `src/wizard_spec_builder.rs:286-372`, `src/wizard.rs:11-22`, `src/cli.rs:2917-2921` |
| Validation rules | Structural required/type/enum validation handled by `greentic-qa-lib` against the JSON form. `validations` arrays are currently empty in all modes. | `src/wizard_spec_builder.rs:125`, `src/wizard_spec_builder.rs:282`, `src/wizard_spec_builder.rs:371`, `src/cli.rs:2919-2933`, `src/cli.rs:2965-2997` |
| Defaults | Form presentation default locale is `en-GB`; CLI defaults include mode=`create`, tenant=`demo`; execution defaults to dry-run unless explicitly executed. | `src/wizard_spec_builder.rs:61`, `src/wizard_spec_builder.rs:145`, `src/wizard_spec_builder.rs:291`, `src/cli.rs:479`, `src/cli.rs:505`, `src/cli.rs:2660-2670`, `docs/wizard.md:10-11` |
| i18n keys | Questions reference `title_i18n.key` entries in `i18n/operator_wizard/*.json`. Loader resolves locale candidates with fallback to `en-GB` then `en`. Tests enforce key presence/sync across locales. | `src/wizard_spec_builder.rs:67`, `src/wizard_spec_builder.rs:74`, `src/wizard_spec_builder.rs:81`, `src/wizard_spec_builder.rs:297`, `src/wizard_i18n.rs:7-42`, `tests/i18n_key_usage.rs:64-79`, `tests/i18n_catalog_sync.rs:41-43` |

## C) Plan / execute / migrate
- [x] Trace plan -> actions path
- [x] Identify side-effects
- [x] Check migration capability

### Findings
| Item | Current approach | Files/lines |
|---|---|---|
| Plan representation | `WizardPlan` contains `mode`, `dry_run`, `bundle`, ordered `steps`, and normalized metadata; created by `apply_create/update/remove`. | `src/wizard.rs:41-84`, `src/wizard.rs:358-460`, `src/wizard.rs:463-674`, `src/wizard.rs:676-794` |
| Apply/execution | `execute_create/update/remove_plan` perform file and state side-effects: create scaffold, fetch/copy packs, update providers registry, write gmap policies, run resolver (`project::sync_project`), copy resolved manifests, remove tenants/packs/providers. | `src/wizard.rs:924-979`, `src/wizard.rs:981-1081`, `src/wizard.rs:1083-1125`, `src/wizard.rs:1143-1193`, `src/wizard.rs:1233-1289`, `src/wizard.rs:1385-1480`, `src/wizard.rs:1535-1590`, `src/wizard.rs:1633-1692`, `src/wizard.rs:1789-1813`, `src/wizard.rs:1848-1884` |
| Validation-only path | No separate validate command. “Validation-only” is operationally dry-run plan generation in CLI (no side effects) plus pre-exec checks (bundle existence / mode checks) in execute methods. | `src/cli.rs:2671-2719`, `src/wizard.rs:838-849`, `src/wizard.rs:928-937`, `src/wizard.rs:988`, `src/wizard.rs:1087` |
| Migration | No AnswerDocument migration mechanism (`--migrate` absent; no migration functions for wizard answers schema). | `src/cli.rs:472-541`, `src/cli.rs:2897-2908`, `src/wizard_spec_builder.rs:1-373` |
| Locks/reproducibility | Deterministic ordering is implemented in planning (`sort/dedup`) and deterministic pack naming. Pack-id mappings are persisted in `.greentic/packs.json` for stable IDs across runs. No explicit “locks” field in wizard answer payload. | `src/wizard.rs:363-383`, `src/wizard.rs:464-483`, `src/wizard.rs:1361-1383`, `src/wizard.rs:1923-2005`, `src/wizard.rs:1983-1985`, `src/cli.rs:550-582` |

Delegation notes (repo -> other execution paths):
- Pack resolution delegates to distributor client library (`greentic_distributor_client`) rather than shelling out to a binary.  
  Ref: `src/wizard.rs:1233-1252`.
- Optional `--run-setup` after wizard execution delegates into existing domain setup pipeline (`run_domain_command`), which then delegates either to `runner_integration` (external runner binary if configured) or `runner_exec` path.  
  Refs: `src/cli.rs:2782-2814`, `src/cli.rs:3969-4007`, `src/cli.rs:5613`, `src/cli.rs:5991-6004`, `src/cli.rs:6210-6231`, `src/cli.rs:6286-6297`.

## D) Tests
- [x] Identify wizard-path tests
- [x] Provide local run commands

### Findings
| Test | What it covers | Command |
|---|---|---|
| `tests/wizard_paths.rs` | End-to-end wizard CLI paths: dry-run answers save, replay execution, overwrite prompt behavior, bundle/resolved/providers outputs. | `cargo test --test wizard_paths` |
| `tests/wizard_i18n_render.rs` | QA UI rendering uses requested locale and locale fallback behavior for question titles. | `cargo test --test wizard_i18n_render` |
| `tests/wizard_i18n_locales.rs` | Locale catalog presence checks for required locale sets in `i18n/operator_wizard`. | `cargo test --test wizard_i18n_locales` |
| `tests/i18n_key_usage.rs` (`wizard_i18n_keys_exist_in_wizard_catalog`) | Ensures all `wizard.*` i18n keys referenced by schema exist in `operator_wizard/en.json`. | `cargo test --test i18n_key_usage wizard_i18n_keys_exist_in_wizard_catalog` |
| `tests/i18n_catalog_sync.rs` (`wizard_catalogs_keep_same_key_set`) | Ensures every wizard locale file has key parity with `en.json`. | `cargo test --test i18n_catalog_sync wizard_catalogs_keep_same_key_set` |
| `src/wizard.rs` unit tests | Determinism, dry-run no-write invariant, create/update/remove execution basics, pack-id mapping behavior. | `cargo test wizard::tests` |

## Ripgrep pattern commands executed
Executed from repo root:

```bash
rg -n "wizard|qa|question|prompt|interactive|inquirer|dialoguer|clap.*wizard|subcommand.*wizard" src docs tests .codex/PR-OP-05-stage1_audit.md
rg -n "schema(_id|_version)?|AnswerDocument|answers\\.json|emit-answers|--answers|--locale|i18n" src docs tests .codex/PR-OP-05-stage1_audit.md
rg -n "apply_answers|setup|default|install|provision|plan|execute|validate|migrate" src docs tests .codex/PR-OP-05-stage1_audit.md
```

## Stage 2-ready inputs (for PR-OP-06)
- Wizard command path(s): `src/cli.rs` top-level and demo alias to `DemoWizardArgs::run`.
- Current flags (locale/answers): `--locale`, `--qa-answers`; no `--answers`/`--emit-answers`; dry-run answer save prompt exists.
- Schema location/model: `src/wizard_spec_builder.rs` JSON form per mode (`id` + `version` = `1.0.0`), with `title_i18n` keys.
- Execution model: CLI builds request -> `wizard_plan_builder` -> `WizardPlan`; optional execute via `wizard_executor`, side-effects in `src/wizard.rs`.
- Tests to update/add: `tests/wizard_paths.rs`, `tests/wizard_i18n_render.rs`, `tests/i18n_key_usage.rs`, `tests/i18n_catalog_sync.rs`, `src/wizard.rs` unit tests.

## Constraints / compatibility notes
- Existing answer artifact is raw `WizardQaAnswers`, not a stable envelope. Backward compatibility likely requires continuing to read this shape when adding AnswerDocument.
- Existing UX relies on `--qa-answers` + interactive save prompt; Stage 2 should preserve these paths while adding aliases (`--answers`, `--emit-answers`) to avoid breaking scripts.
- There is no current explicit migrate path; introducing `--migrate` should default to no-op for current `1.0.0` answer payloads.
