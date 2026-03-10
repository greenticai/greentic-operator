# Domain Contracts (Common)

This document defines the operator expectations for provider packs across all domains.
Provider packs are distributed as `.gtpack` files and are discovered by the operator
under `providers/<domain>/*.gtpack`.

## Required folder layout

```
<project>/
  providers/
    <domain>/
      <provider>.gtpack
```

## Standard lifecycle flows

Provider packs may expose the following entry flows:

- `setup_default` (required unless explicitly allowed to be missing)
- `setup_custom` (optional)
- `diagnostics` (recommended)
- `verify_*` (optional domain-specific verification flows)

If `diagnostics` or `verify_*` is missing, the operator skips it and records a warning.
If `setup_default` is missing, the operator fails unless `--allow-missing-setup` is set.

## Input payload conventions

Operator calls provide a minimal JSON payload:

```json
{
  "tenant": "TENANT_ID",
  "team": "TEAM_ID"
}
```

`team` is omitted when not provided. Domain-specific fields may be added in the future:

- `public_base_url` if available (for webhook verification flows)

## Output expectations

Operator captures run output and artifacts under:

```
state/runs/<domain>/<pack>/<flow>/<timestamp>/
  run.json
  summary.txt
  artifacts_dir
```

Provider packs should ensure flow outcomes can be reflected in the runner `RunResult`
and that failures are surfaced via non-success status.

## Secrets helpers

Operator workflows delegate secret orchestration to `greentic-secrets`. Run:

```
gtc op demo secrets init --tenant <TENANT> --team <TEAM> --pack <PATH_TO_PROVIDER_PACK>
```

and the CLI will forward the command to `greentic-secrets` with the current `--env`/`--tenant`/`--team`, streaming stdout/stderr to the terminal and to `state/logs/secrets/init-<timestamp>.log`. `demo secrets set/get/list/delete` behave the same and support `--secrets-bin` overrides or binary mappings from `greentic.yaml`.

`greentic-operator dev setup messaging` (and `demo setup`/`demo build`) calls secrets init for every provider pack before running requirements, so missing secrets surface early along with instructions to run `gtc op demo secrets set <NAME>`. The logs in `state/logs/secrets/` capture the full greentic-secrets transcript for troubleshooting.
