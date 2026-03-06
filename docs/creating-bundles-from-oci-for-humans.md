# Creating Bundles from OCI Providers for Humans

This guide standardizes how to create/update bundles via `greentic-operator demo wizard`, including providers from OCI.

## Rule of thumb

Use wizard-first for bundle lifecycle:

- create bundle
- add provider packs from registry/catalog/custom refs
- apply allow rules
- optionally run setup

## 1) Create new bundle (interactive)

```bash
greentic-operator demo wizard \
  --mode create \
  --bundle ./mybundle \
  --provider-registry oci://ghcr.io/greenticai/registries/providers:latest \
  --tenant demo \
  --team default
```

By default this is plan-only (dry-run) and writes answers.

Execute explicitly:

```bash
greentic-operator demo wizard \
  --mode create \
  --bundle ./mybundle \
  --provider-registry oci://ghcr.io/greenticai/registries/providers:latest \
  --execute
```

## 2) Add providers from OCI/repo/store/custom refs

Use repeatable `--pack-ref`:

```bash
greentic-operator demo wizard \
  --mode update \
  --bundle ./mybundle \
  --pack-ref oci://ghcr.io/acme/providers/messaging-telegram:1.2.3 \
  --pack-ref repo://greenticai/providers/events-github:latest \
  --pack-ref store://providers/secrets-dev:stable \
  --execute
```

For non-well-known providers, include custom refs through wizard answers or `--pack-ref`.

## 3) Add catalog providers by ID

```bash
greentic-operator demo wizard \
  --mode update \
  --bundle ./mybundle \
  --catalog-pack messaging.telegram \
  --catalog-pack events.github \
  --execute
```

## 4) Apply target-specific allow rules

```bash
greentic-operator demo wizard \
  --mode update \
  --bundle ./mybundle \
  --target demo:default \
  --allow app.weather/main/send \
  --execute
```

## 5) Run provider setup after execution

```bash
greentic-operator demo wizard \
  --mode create \
  --bundle ./mybundle \
  --execute \
  --run-setup
```

Optionally preload setup answers:

```bash
greentic-operator demo wizard \
  --mode update \
  --bundle ./mybundle \
  --execute \
  --run-setup \
  --setup-input ./setup-input.yaml
```

## Offline behavior

Use cached artifacts only:

```bash
greentic-operator demo wizard --mode update --bundle ./mybundle --offline
```

## Output guarantee

Execution reuses the same demo lifecycle as allow/forbid commands:

- updates gmap
- reruns resolver
- copies resolved manifest for demo start
