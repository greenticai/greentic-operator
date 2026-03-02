# greentic-operator: Stage 2 Surgical Update (PR-OP-06)

Pre-req: Stage 1 audit completed and findings captured in `.codex/PR-OP-05-stage1_audit.md`.

## Objective
Implement the minimal set of changes to support:
- Stable AnswerDocument import/export (envelope)
- Schema identity + version (`schema_id`, `schema_version`) for this wizard
- Non-interactive execution via `--answers <file>`
- Optional migration via `--migrate`
- i18n keys (schema uses keys; labels resolved by locale)
- Preserve/alias existing CLI paths where required

## Repo description
Operator-level wizard; provisioning/setup flows; validates/applies AnswerDocuments

## Audit Inputs (from Stage 1)
- Wizard command path(s): top-level `wizard` and `demo wizard` both invoke `DemoWizardArgs::run`.
  - Refs: `src/cli.rs:85-92`, `src/cli.rs:102-134`, `src/cli.rs:1724-1727`, `src/cli.rs:1764`
- Current flags (locale/answers): `--locale` (global + wizard-local), `--qa-answers` import; no `--answers`/`--emit-answers`; dry-run prompts and writes raw answers file.
  - Refs: `src/cli.rs:79-83`, `src/cli.rs:484-487`, `src/cli.rs:533-534`, `src/cli.rs:2707-2718`, `src/cli.rs:2841-2861`
- Schema location/model: JSON validation forms in `src/wizard_spec_builder.rs`, IDs (`operator.wizard.*`) and `version: "1.0.0"`, with `title_i18n` keys.
  - Refs: `src/wizard_spec_builder.rs:58-61`, `src/wizard_spec_builder.rs:142-145`, `src/wizard_spec_builder.rs:288-291`
- Execution model (plan/apply): `wizard_plan_builder::build_plan` -> `wizard_executor::execute`; side effects in `src/wizard.rs` execution functions.
  - Refs: `src/wizard_plan_builder.rs:7-9`, `src/wizard_executor.rs:4-8`, `src/wizard.rs:924-1125`
- Tests to update/add:
  - Existing: `tests/wizard_paths.rs`, `tests/wizard_i18n_render.rs`, `tests/wizard_i18n_locales.rs`, `tests/i18n_key_usage.rs`, `tests/i18n_catalog_sync.rs`, `src/wizard.rs` unit tests.

## Proposed changes (minimal)
### 1) Add/standardize AnswerDocument envelope
- Add a local wizard AnswerDocument struct:
  - `wizard_id`, `schema_id`, `schema_version`, `locale`, `answers`, `locks`
- Keep `answers` payload compatible with current `WizardQaAnswers` fields.
- Read path must accept:
  - envelope JSON (new)
  - legacy raw `WizardQaAnswers` JSON/YAML (existing)
- Write path emits envelope by default; keep legacy compatibility by continuing to accept old input.

### 2) CLI flags + semantics (surgical)
Implement/alias:
- `--answers <FILE>`: new alias to non-interactive answer input (envelope preferred)
- `--emit-answers <FILE>`: new explicit output path for writing AnswerDocument
- `--schema-version <VER>`: pin schema version embedded in emitted envelope (and used in migration checks)
- `--migrate`: if input document has older schema version, migrate (identity for now) and continue

Compatibility rule:
- Keep `--qa-answers` and map it internally as a compatibility alias for `--answers`.
- Keep existing default dry-run prompt behavior if `--emit-answers` is not supplied.

### 3) Schema identity + versioning
Use stable IDs:
- `wizard_id`: `greentic-operator.wizard.demo`
- `schema_id`: `greentic-operator.demo.wizard`
- `schema_version`: default `1.0.0` (matches current form `version`)

Ensure emitted AnswerDocument always contains these fields.

### 4) Validate vs apply split
Current model is execute-vs-dry-run flags, not subcommands.
Minimal adaptation:
- Add `--validate` and `--apply` flags as semantic aliases:
  - `--validate` => plan/validation only (no side effects)
  - `--apply` => execute side effects
- Keep existing `--execute` / `--dry-run` behavior and precedence for compatibility.

### 5) Migration
- Implement a migration hook:
  - Input: AnswerDocument
  - Output: AnswerDocument
- Current implementation: identity migration for `1.0.0`, with explicit version checks and actionable errors when `--migrate` is absent.

### 6) i18n wiring
- Keep current schema `title_i18n` usage in `wizard_spec_builder`.
- Keep locale affecting only QA rendering and envelope `locale`, not stable answer keys/values.

## Acceptance criteria
- [ ] Existing interactive wizard flow still works with no new flags
- [ ] `wizard --validate --answers answers.json` works with no side effects
- [ ] `wizard --apply --answers answers.json` works with side effects
- [ ] `wizard --emit-answers out.json` produces AnswerDocument envelope with ids/versions
- [ ] `wizard --answers old.json --migrate` succeeds for known older/equivalent versions and can re-emit
- [ ] Existing tests continue passing; new focused tests cover envelope import/export and alias compatibility

## Implementation notes (from audit)
- Files to touch:
  - `src/cli.rs` (flags, parsing, envelope I/O, validate/apply aliases, migrate wiring)
  - Optional: `src/wizard_spec_builder.rs` (if schema version pinning needs explicit sync point)
  - Optional docs: `docs/wizard.md` (new flags + envelope behavior)
- Tests to touch/add:
  - Update/add in `tests/wizard_paths.rs` for:
    - `--answers` alias
    - `--emit-answers` output envelope
    - validate/apply alias behavior
  - Add migration path coverage (new test in `tests/wizard_paths.rs` or dedicated file)
  - Keep i18n/catalog tests unchanged unless keys/docs change

## Risk controls
- No large refactors; keep changes localized to wizard CLI plumbing
- Preserve existing UX defaults (`--qa-answers`, dry-run output prompt)
- Avoid schema mega-merges; keep answer payload nested in envelope

## Common target behavior (all repos)

**Goal:** Standardize wizard execution and portability via a stable AnswerDocument envelope and consistent CLI semantics, while keeping schema ownership local to each wizard.

### AnswerDocument envelope (portable JSON)
```json
{
  "wizard_id": "greentic.pack.wizard.new",
  "schema_id": "greentic.pack.new",
  "schema_version": "1.1.0",
  "locale": "en-GB",
  "answers": { "...": "..." },
  "locks": { "...": "..." }
}
```

### Required CLI semantics
All wizards should converge on these flags and semantics (names can vary only if you provide compatibility aliases):
- `--locale <LOCALE>`: affects i18n rendering only; **answers must remain stable IDs/values**
- `--answers <FILE>`: non-interactive input (AnswerDocument)
- `--emit-answers <FILE>`: write AnswerDocument produced (interactive or merged)
- `--schema-version <VER>`: pin schema version used for interactive rendering/validation
- `--migrate`: allow automatic migration of older AnswerDocuments (including nested ones where applicable)
- Separate `validate` vs `apply` paths (subcommands or flags), recommended:
  - `wizard validate --answers ...`
  - `wizard apply --answers ...`

### Versioning rules
- Patch/minor: backwards compatible additions (defaults) only
- Major: breaking changes require migration logic
- Avoid flattening composed schemas into one mega-schema; prefer nested AnswerDocuments for composed flows.

### i18n rules
- Schema uses i18n keys; runtime resolves by locale
- Answers never depend on localized labels; only stable values/IDs

Date: 2026-03-02
