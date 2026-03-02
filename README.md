# greentic-operator

Greentic Operator orchestrates a project directory for demos and local development.
It manages tenants/teams, access mapping (.gmap), pack/provider discovery, resolved manifests, and starting local runtime services.

## Quickstart (demo)

```bash
mkdir my-demo && cd my-demo
greentic-operator demo new demo-bundle
# drop provider packs into providers/messaging/ and providers/events/
# drop packs into packs/
greentic-operator demo setup --bundle demo-bundle --tenant default --team default
greentic-operator demo build --bundle demo-bundle --tenant default --team default
greentic-operator demo start --bundle demo-bundle --tenant default --team default
```

`demo start` is the canonical, long-running invocation: it boots the demo services in the foreground and waits for **Ctrl+C** to trigger a clean shutdown sequence. Press **Ctrl+C** in the terminal running the command to stop the services.

Access mapping (.gmap)

Rules are line-oriented:

<path> = <policy>

Paths:

_ default

pack_id, pack_id/_, pack_id/flow_id, pack_id/flow_id/node_id

Policies (MVP):

public

forbidden

Team rules override tenant rules.

Demo bundles

greentic-operator demo build --out demo-bundle --tenant tenant1 --team team1
greentic-operator demo start --bundle demo-bundle --tenant tenant1 --team team1
Note: demo bundles require CBOR-only packs (`manifest.cbor`). Rebuild packs with `greentic-pack build` (avoid `--dev`).

### allow/forbid commands

There are two sets of gmap editing helpers:

- `greentic-operator demo allow/forbid` is meant for portable bundles. Supply `--bundle <DIR>` plus `--tenant`/`--team` and pass the same `PACK[/FLOW[/NODE]]` path. The command rewrites the bundle’s gmap, reruns the resolver, and copies the updated `state/resolved/<tenant>[.<team>].yaml` into `resolved/`, so `demo start` immediately sees the change.

Paths must contain at most three segments. Passing `PACK/FLOW/NODE/EXTRA` (or relative paths with more than three parts) will trigger the “too many segments” error you saw. Stick to the `pack`, `pack/flow`, or `pack/flow/node` forms.

Demo send (generic)

greentic-operator demo send --bundle demo-bundle --provider telegram --print-required-args
greentic-operator demo send --bundle demo-bundle --provider telegram --text "hi" --arg chat_id=123
greentic-operator demo send --bundle demo-bundle --provider telegram --card cards/welcome.json --arg chat_id=123

Demo new (bundle scaffold)

greentic-operator demo new demo-bundle
greentic-operator demo new demo-bundle --out /tmp

Creates the directory layout plus minimal metadata (`greentic.demo.yaml`, `tenants/default/tenant.gmap`, `providers/*`, `state`, `resolved`, `logs`, etc.) so you can start adding packs and tenant definitions before running `demo setup`/`demo build`.

Demo receive (incoming)

Terminal A: `greentic-operator demo receive --bundle demo-bundle`
Terminal B: `greentic-operator demo send --bundle demo-bundle --provider telegram --text "hi" --arg chat_id=123`

`demo receive` listens for the bundle's messaging ingress subjects, streams each message to stdout, and appends a JSON line to `incoming.log`. Use `--provider` to focus on a single provider or `--all`/default to watch every enabled messaging pack.

### demo ingress (synthetic HTTP)

`greentic-operator demo ingress` lets you exercise the universal HTTP ingress and operator outbound pipeline without running a full HTTP gateway. It constructs an `HttpInV1` body, invokes the provider `ingest_http` flow, prints the HTTP response plus any `ChannelMessageEnvelope` events, and (with `--end-to-end`) pushes the events through the app + render/encode/send flow.

Key flags:

- `--provider <name>` (required): provider pack or ID (slack, telegram, teams, webex, whatsapp, webchat, email, dummy, etc.).
- `--bundle <dir>` (required): demo bundle directory containing the provider packs.
- `--path <path>`: overrides the default `/ingress/<provider>/webhook` path (or `/ingress/<provider>/<binding_id>` when `--binding-id` is set).
- `--method <GET|POST>`: choose the HTTP method (default `POST`).
- `--body`, `--body-json`, `--body-raw`: body source (only one allowed).
- `--binding-id <id>`: populate the binding ID/route for Telegram-style callbacks.
- `--print <http|events|all>`: control printed output (defaults to `all`).
- `--end-to-end`: invoke `render_plan`, `encode`, and `send_payload` for each event.
- `--send`/`--dry-run`: choose whether `send_payload` actually fires (default dry-run when end-to-end).
- `--retries <n>`: cap the retry attempts when `send_payload` returns `node-error`.
- `--dlq-tail`: prints the shared `dlq.log` path (same file the runtime uses).

Use `--tenant`/`--team`/`--correlation-id` to simulate the context headers that would arrive via a real gateway. Add `--app-pack` to target a custom app pack override instead of the demo’s default selection.

## Domain auto-discovery

Domains are enabled automatically when provider packs exist:

- messaging: `providers/messaging/*.gtpack`
- events: `providers/events/*.gtpack`

You can override per-domain behavior in `greentic.yaml`:

```yaml
services:
  messaging:
    enabled: auto   # auto|true|false
  events:
    enabled: auto   # auto|true|false
```

## Dev/demo dependency mode

Dev/demo uses local path dependencies for greentic-* crates with `version = "0.4"` and
`path = "../<repo>"`. Publishing (future) requires stripping path deps and relying on
registry-only versions.

## Legacy commands

Everything under `greentic-operator dev …` is legacy.

- `greentic-operator dev on|off|status`
- `greentic-operator dev up|down|embedded`
- `greentic-operator dev logs|svc-status`

Other dev subcommands remain callable but are hidden from `--help`.

## Local dev binaries

When iterating in a workspace/monorepo, you can resolve binaries from local build outputs
instead of relying on `cargo binstall` or `$PATH`:

```bash
greentic-operator dev on --root /projects/ai/greentic-ai --profile debug
greentic-operator dev detect --root /projects/ai/greentic-ai --profile debug --dry-run
greentic-operator demo start --bundle demo-bundle --tenant default --team default
greentic-operator demo doctor
```

Config (greentic.yaml) supports dev mode defaults and explicit binary overrides:

```yaml
dev:
  mode: auto
  root: /projects/ai/greentic-ai
  profile: debug
  target_dir: null
  repo_map:
    greentic-pack: greentic-pack
    greentic-secrets: greentic-secrets
binaries:
  greentic-pack: /custom/bin/greentic-pack
```

Resolution order (hybrid dev-mode):
1) Explicit config path (binaries map or command path).
2) Dev-mode repo_map override under `dev.root` (if enabled).
3) Fallbacks (`./bin`, `./target/*`, then `$PATH`).

Global dev-mode settings are stored in `~/.config/greentic/operator/settings.yaml` (platform-
appropriate equivalents on macOS/Windows). Use `greentic-operator dev status` to view them and
`greentic-operator dev off` to disable dev mode globally.

## Demo service config

`greentic-operator demo start` reads the `services` section of `greentic.yaml` to decide which gateway/egress/subscriptions components to launch, but demo bundles no longer copy or depend on the `gsm-*` binaries listed in earlier docs. The operator now runs embedded implementations of the gateway/egress/subscriptions services by default, so you only need to override `services.gateway.binary`, `services.egress.binary`, or `services.subscriptions.*.binary` when pointing to a custom executable outside the embedded runtime. By default, demo start does **not** spawn local NATS (`--nats=off`), but you can opt into the legacy GSM NATS stack with `--nats=on` (this prints a warning) or attach to an external NATS server via `--nats=external --nats-url <URL>`.

When the demo bundle exposes a gateway host/port (either via `greentic.demo.yaml` or `greentic.yaml`), an always-on HTTP ingress server listens on `http://<gateway-listen-addr>:<gateway-port>` and routes any POST/GET to `/{domain}/ingress/{provider}/{tenant}/{team?}` through the runner-host flows (`handle-webhook` ➜ `ingest`). Responses include the flow outcome (success, mode, outputs, errors) as structured JSON and are logged alongside the existing `demo receive` pipeline. `demo start` also logs `embedded runner mode; gateway/egress disabled` when it avoids launching the legacy GSM services, so the CLI stays on the embedded path unless `--nats=on` is explicitly requested.

## Webhook tunneling

`demo start` can automatically spawn a public tunnel so that external services (Telegram, Slack, Teams, etc.) can deliver webhooks to your local machine. Two tunnel backends are supported: **Cloudflare Tunnel** (default) and **ngrok**.

| Flag | Default | Description |
|------|---------|-------------|
| `--cloudflared <on\|off>` | `on` | Start a Cloudflare quick tunnel (`*.trycloudflare.com`). |
| `--cloudflared-binary <PATH>` | — | Explicit path to the `cloudflared` binary. |
| `--ngrok <on\|off>` | `off` | Start an ngrok tunnel (`*.ngrok-free.app`). |
| `--ngrok-binary <PATH>` | — | Explicit path to the `ngrok` binary. |

Only one tunnel should be active at a time. To use ngrok instead of cloudflared:

```bash
greentic-operator demo start --bundle demo-bundle \
  --cloudflared off --ngrok on
```

The discovered public URL is written to `state/runtime/<tenant>/<team>/public_base_url.txt` and injected into provider setup inputs automatically. Both backends can be restarted via `--restart ngrok` or `--restart cloudflared`.

Binary resolution follows the standard order: explicit `--*-binary` flag, `GREENTIC_<NAME>` env var, `<bundle>/bin/`, `<bundle>/target/{debug,release}/`, then `$PATH`.

## Demo subscriptions mode

`greentic-operator demo start` defaults to the embedded universal subscriptions scheduler. Use `services.subscriptions.mode` in `greentic.yaml` to switch between the legacy GSM binary and the provider-op driven implementation:

```yaml
services:
  subscriptions:
    mode: universal_ops        # universal_ops | legacy_gsm
    universal:
      renew_interval_seconds: 60
      renew_skew_minutes: 10
      desired:
        - provider: email
          resource: "/me/mailFolders('Inbox')/messages"
          change_types: ["created", "deleted"]
          notification_url: "https://example.com/ingress/email/subscriptions/<binding_id>"
          client_state: "demo-${provider}"
          user:
            user_id: "alice@example.com"
            token_key: secrets://demo/default/messaging/alice_refresh_token
```

The universal scheduler calls the provider `subscription_*` ops (using the shared `messaging_universal_dto` contract) and persists binding metadata under `state/subscriptions/<provider>/<tenant>/<team>/<binding_id>.json`. The stored `AuthUserRefV1` is reused when renewing or deleting subscriptions, which keeps delegated email/Teams contexts intact.

Use `greentic-operator demo subscriptions` to manage bindings manually:

- `demo subscriptions ensure` invokes `subscription_ensure`, stores the binding, and prints its path.
- `demo subscriptions status` lists persisted bindings plus expiry timestamps.
- `demo subscriptions renew` runs the scheduler (either all due bindings or a single `--binding-id`) and re-writes state.
- `demo subscriptions delete` calls `subscription_delete` and removes the stored file.

These commands are handy for smoke testing provider packs and delegated scenarios without running a full demo stack.

Snapshot `docs/demo-universal-subscriptions.yaml` contains a ready-to-use `greentic.demo.yaml` snippet you can drop into a bundle before running `demo start --subscriptions-mode universal_ops`.
