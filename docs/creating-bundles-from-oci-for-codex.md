# Creating Bundles from OCI Providers for Codex

This guide is the deterministic Codex workflow for `greentic-operator demo wizard`.

## Deterministic contract

Always use:

1. plan/validate (no side effects)
2. emitted AnswerDocument
3. apply/execute using the same answers

## Record answers (plan-only)

```bash
greentic-operator demo wizard \
  --mode create \
  --bundle ./mybundle \
  --provider-registry oci://ghcr.io/greenticai/registries/providers:latest \
  --validate \
  --emit-answers .codex/operator-wizard.answers.json
```

## Validate/migrate answers

```bash
greentic-operator demo wizard \
  --mode create \
  --answers .codex/operator-wizard.answers.json \
  --migrate \
  --validate \
  --emit-answers .codex/operator-wizard.answers.normalized.json
```

## Execute from answers

```bash
greentic-operator demo wizard \
  --mode create \
  --answers .codex/operator-wizard.answers.normalized.json \
  --execute
```

## AnswerDocument envelope

```json
{
  "wizard_id": "greentic-operator.wizard.demo",
  "schema_id": "greentic-operator.demo.wizard",
  "schema_version": "1.0.0",
  "locale": "en",
  "answers": {},
  "locks": {}
}
```

## High-value `answers` keys for Codex

- `bundle_path` or `bundle`
- `catalog_packs`
- `pack_refs`
- `providers`
- `custom_provider_refs`
- `tenant`, `team`, `targets`
- `allow_paths`
- `execution_mode` (`"dry run"` or `"execute"`)
- `locale`

## Example `answers` payload snippet

```json
{
  "bundle_path": "./mybundle",
  "catalog_packs": ["messaging.telegram"],
  "pack_refs": [
    {
      "pack_ref": "oci://ghcr.io/acme/providers/events-github:1.0.0"
    }
  ],
  "targets": [
    { "tenant_id": "demo", "team_id": "default" }
  ],
  "allow_paths": ["app.weather/main/send"],
  "execution_mode": "execute",
  "locale": "en"
}
```

## Recommended automation sequence

1. Generate/refresh answer doc with `--validate --emit-answers`
2. Commit answer docs in repo (or pipeline artifacts)
3. Execute with `--answers ... --execute`
4. Optionally append `--run-setup --setup-input <FILE>`

## i18n compliance note

When changing wizard text/flags/help, follow:

- [cli-i18n-codex-playbook.md](/home/vgrishkyan/greentic/greentic-i18n/docs/cli-i18n-codex-playbook.md)

Run translation updates in batches and integrate with local checks.
