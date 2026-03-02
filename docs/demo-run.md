# Demo Run & Discovery Guide

## Discovering packs

Use `demo list-packs` to see what provider packs are installed for a domain:

```
greentic-operator demo list-packs --bundle /path/to/bundle --domain messaging
```

The command prints each `pack_id`, how many entry flows it exposes, and the filename under `providers/messaging`.

## Inspecting flows

To learn which flows a pack exposes, run:

```
greentic-operator demo list-flows --bundle /path/to/bundle --pack messaging-telegram --domain messaging
```

That prints the entry flow IDs you can pass via `--flow` to `demo run` or `demo send`.

## Running flows

The `demo run` command executes a manifest flow with inline JSON input:

```
greentic-operator demo run --packs-dir ./packs --pack messaging-telegram --tenant demo --flow default --input '{"trigger":"start"}'
```

The command prints a short run summary, including which pack/flow/tenant were used and the input source, followed by the flow result and exit status.

## Filling forms and clicking actions

Once a flow blocks on an adaptive card, you interact through the REPL commands documented in `demo send`:

- `@show`: reprints the last card summary.
- `@json`: dumps the raw JSON payload for the waiting card.
- `@input <field>=<value>`: stores a value for the named input (available fields are listed when you mistype the id).
- `@click <action_id>`: fires the named action (invalid ids also show the available actions).
- `@setup [provider]`: runs the FormSpec-driven QA wizard for a provider pack. When called without an argument, uses the current pack. The wizard builds a FormSpec from the pack's `setup.yaml` (or WASM `qa-spec` op), interactively prompts for each field with type hints and validation, then persists secrets to the dev store and non-secret config to the provider config envelope.
- `@back`: restores the previous blocked card and pending inputs.
- `@help`: prints the quick command reference.
- `@quit`: exits the REPL.

The hints printed for `@input`/`@click` make it easy to know which identifiers are accepted, and `@back` lets you revisit the previous card state before submitting.

## Event semantics

The REPL simply emits user events (card submissions) into the same flow engine the operator uses elsewhere. `@click` with stored inputs is converted into an `action_id` plus the `inputs` map and treated just like an incoming event, so future integrations can replace the REPL with real messaging providers without touching the flow execution logic.
