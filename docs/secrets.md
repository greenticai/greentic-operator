# Secrets Workflow

`greentic-operator` now has a single deterministic workflow for dev secrets:

1. A dev store lives under the bundle root (`.greentic/dev/.dev.secrets.env`) or can be overridden with `GREENTIC_DEV_SECRETS_PATH`. On the first use the directory tree is created automatically; subsequent runs reuse the same store.
2. When you run a setup flow (`demo setup`, `demo up`, `domain setup`, etc.) the operator spins up a small Tokio runtime, opens the dev store with `DevStore::with_path`, and calls `SecretsSetup::ensure_pack_secrets`. That method:
   * scans each pack for `secret_requirements` assets or manifest entries, canonicalizes the URIs (`secrets://{env}/{tenant}/{team}/{provider}/{key}`), and checks whether the URI already exists in the store.
   * if the URI is missing it attempts to seed a value from `seeds.yaml` (at the bundle root or in `<bundle>/state/seeds.yaml`). If no entry is found it writes a fast placeholder such as `placeholder for secrets://...` so the store is never empty.
   * logs canonicalization and backend selection via `tracing` so `RUST_LOG=greentic_secrets_repro=debug` (used by the repro crate) displays the URIs and the store path.
3. After seeding the runtime continues with the usual runner-based setup flows. `--skip-secrets-init` (or `SKIP_SECRETS_INIT`) lets you skip the seeding step when needed.
4. The same dev store is later consumed by `SecretsManagerHandle`, so reads (e.g., when running flows that expect secrets) resolve against the freshly seeded data.

## Configuring secrets values

* Place a `seeds.yaml` file next to `greentic.demo.yaml` (or under `<bundle>/state/seeds.yaml`). Its format mirrors `greentic-secrets` seed documents (`entries: [{ uri: ..., format: text, value: { type: text, text: ... } }]`).
* If you just care about the store layout, run `demo setup --skip-secrets-init` (or `domain setup --skip-secrets-init`).
* Set `GREENTIC_DEV_SECRETS_PATH` to write/read a store from a custom location.
* Override the environment used for canonical URIs with `--secrets-env <ENV>` (defaults to `GREENTIC_ENV` or `dev`).

The accompanying `crates/greentic-secrets-repro` crate proves this dev store path is deterministic, seeds two different provider URIs, validates NotFound messages, and reopens the store to assert persistence. Run it with `RUST_LOG=greentic_secrets_repro=debug cargo test -p greentic-secrets-repro -- --nocapture` to exercise the same logic.

## Mental model (1 minute)

  * Secrets live in one dev store rooted at `.greentic/dev/.dev.secrets.env` (or `GREENTIC_DEV_SECRETS_PATH` when overridden).
  * `SecretsSetup::ensure_pack_secrets` is the only place that canonicalizes URIs, checks the store, and writes missing entries.
  * Everything else (flows, CLI commands, provider execution) reads secrets from that store via `SecretsManagerHandle`; there is no additional init/apply split, nor are there legacy lookup fallbacks.

## Quickstart (3 steps)

  1. Run `greentic-operator setup --bundle <BUNDLE>` (or `demo setup`, `domain setup`, etc.) so the operator opens the dev store, scans packs/providers for secret requirements, and seeds missing URIs from `seeds.yaml` (or placeholders).
  2. Confirm the generated store contains the expected URIs and values (`.greentic/dev/.dev.secrets.env` or by rerunning `demo setup --skip-secrets-init` to view the structure).
  3. Use `gtc op demo send` / flows / other CLI operations; they now resolve secrets from the same dev store, and missing keys are loudly reported with the URI, env-style key, store path, and a hint to rerun setup or edit `.dev.secrets.env`.

## What not to do anymore

  * Don’t run separate “init” and “apply” commands for secrets; the `setup` flows (demo/domain/etc.) now seed and resolve secrets in one pass.
  * Don’t add ad-hoc fallback lookups against other namespaces/backends; every secret must use the canonical URI/`GREENTIC_SECRET__…` key defined by `SecretsSetup`.
  * Don’t rely on implicit provider inference; all required keys come from pack manifests or `secret_requirements` assets, and automated placeholders keep the dev store consistent.
