PR-OP-04-hooks-subs
Offer Registry + Hook/Subs Execution from GTPack pack.cbor (Path-Agnostic, Production-Ready)

Repo: greentic-operator
Date: 2026-02-25
Status: New feature
Constraints: CBOR-only runtime, gtpack zip + pack.cbor is source of truth

Goal

Add a generic Offer Registry that is populated purely from installed gtpacks’ pack.cbor, and use it to support:

Hooks (pipeline interceptors) discovered dynamically

Subs (event subscriptions) discovered dynamically

Capabilities (existing or future) discovered dynamically

No linkage to filesystem location conventions.

Non-goals

No changes to greentic-interfaces, greentic-qa, or provider repos

No JSON assets at runtime

No bespoke “routing” logic in operator beyond applying a hook directive (generic control)

High-level behavior
Offer discovery (new)

When operator loads installed .gtpack zip files:

Read pack.cbor

Extract offers[]

Register each offer into an in-memory registry keyed by:

offer.kind (hook | subs | capability)

and for hooks/subs: additionally by stage + contract (if present)

Key point: Operator never infers meaning from where the pack is stored.

Hook execution (new)

During ingress handling (messaging/events):

Normalize inbound → internal envelope

Run existing pre-validation/security (unchanged)

Run hook stage: post_ingress

Select hook offers where:

kind == "hook"

stage == "post_ingress"

contract == "greentic.hook.control.v1"

Order: priority ASC, tie-breaker offer.id ASC

Invoke in order until one returns a directive with action != "continue", then stop and apply it

If no hook short-circuits, proceed with normal dispatch

Subs execution (discovery only in this PR, optional activation)

This PR should at least discover subs offers and store them.
(You can choose to execute subs in a follow-up PR to keep this surgical; see scope section.)

Offer model (operator internal)

Add/extend operator’s internal DTO for offers (do not change external repos):

Offer {
  id: string                    // required, stable
  kind: "hook" | "subs" | "capability"
  priority: int                 // default 100 if missing
  provider: { op: string }      // required: op to invoke in the pack/component
  stage: string?                // required for hook/subs
  contract: string?             // required for hook/subs
  meta: map?                    // optional
}
Contract constants (operator)

Add:

HOOK_STAGE_POST_INGRESS = "post_ingress"

HOOK_CONTRACT_CONTROL_V1 = "greentic.hook.control.v1"

Hook directive format (CBOR, production-safe, minimal)

The invoked hook returns result_cbor as CBOR. Operator parses only:

Required

action: "continue" | "dispatch" | "respond" | "deny"

Dispatch

target: string "tenant/team/pack[/flow[/node]]" OR structured map

params: map? (opaque, passed through)

hints: map? (opaque, passed through)

Respond

response_text: string?

response_card: CBOR map (not JSON) representing the card payload (operator treats it as opaque structured data)

needs_user: bool? default true

Deny

reason: map? {"code":"...", "text":"..."}

optional response fields

Malformed handling (production)

If CBOR decode fails OR missing/invalid action → treat as continue

Emit structured audit log for observability

(If you later want fail-closed per tenant/team policy, that’s a separate policy PR.)

Implementation plan
1) Build an Offer Registry populated from pack.cbor

Files (suggested)

greentic-operator/src/packs/loader.rs (where gtpack zip is read)

greentic-operator/src/offers/mod.rs (new)

greentic-operator/src/offers/registry.rs (new)

Work

When loading each pack:

parse pack.cbor

extract offers[] if present

register each offer under (kind, stage?, contract?)

Store origin_pack_id and origin_pack_ref (for invoke routing and logs)

Acceptance

Operator can list discovered offers (debug log / doctor command if present)

2) Hook invocation abstraction

Files

greentic-operator/src/hooks/mod.rs (new)

greentic-operator/src/hooks/runner.rs (new)

Work

Implement:

select_hooks(stage, contract) -> Vec<OfferRef>

stable ordering: priority ASC, id ASC

invoke_hook(offer, envelope) -> result_cbor

Invocation uses existing pack/component invocation mechanism (whatever operator already uses to call an op inside a pack).

3) Directive parsing + apply step in ingress pipeline

Files

greentic-operator/src/ingress/mod.rs

greentic-operator/src/ingress/control_directive.rs (new)

greentic-operator/src/ingress/pipeline.rs (or current pipeline file)

Work

try_parse_control_directive(result_cbor) -> ControlDirective

apply_control_directive(directive, envelope) -> Outcome

dispatch target resolution uses existing resolver

respond uses existing channel response pipeline

deny stops with optional response

Idempotency

If a hook produces dispatch/respond/deny, ingress pipeline MUST NOT do default dispatch afterwards.

4) Observability (production)

Emit structured logs:

offer.registry.loaded (counts by kind)

hook.invoked (offer id, stage, contract)

hook.directive.applied (action, target)

hook.directive.parse_error (offer id, error)

5) Tests

Unit

Offer registry loads offers from a synthetic pack.cbor

Hook ordering determinism (priority + id)

Directive parsing cases including malformed CBOR -> continue

Integration-ish

Fake hook offer invocation returns predetermined CBOR:

continue → falls through

dispatch → short-circuits

respond → short-circuits

deny → short-circuits

Scope decision for subs (keep simple)

This PR should:

✅ Discover and register subs offers

✅ Provide selector API for subs (but not necessarily execute them)

Then a follow-up PR can implement “subs execution” once hook path lands cleanly.

Acceptance criteria

Operator discovers offers from installed gtpack pack.cbor (no path conventions)

Operator can run post_ingress hooks for greentic.hook.control.v1

Hook directives can short-circuit ingress with dispatch/respond/deny

No changes needed in other repos to add new hook packs (they just ship a conforming offer)

Locked decisions (production-ready, minimal scope)

1) Offer id uniqueness and registry keying

offer.id is unique per pack (not globally).

Registry canonical key is:

offer_key = "{pack_id}::{offer.id}"

Validation rules:

- Duplicate offer.id within one pack: hard error (pack malformed)
- Duplicate pack_id across installed packs: hard error
- Same offer.id across different packs: allowed (namespaced)

2) Dispatch target normalization

Hook output may provide dispatch.target as string shorthand, but operator immediately normalizes into:

DispatchTarget { tenant, team, pack, flow?, node? }

Only the structured form is used after parsing/validation.

3) Reply/deny normalization contract

Internal normalized type:

IngressReply { text?, card_cbor?, status_code?, reason_code? }

Behavior:

- respond: normal outbound reply path, default success (200-equivalent)
- deny: rejection path, default denied (403-equivalent), optional user-facing reply

card payload is treated as CBOR map (opaque), not JSON.

4) Hook execution scope

Messaging ingress: enabled in this PR.

Events ingress:

- include if it already shares ingress pipeline safely, or
- gate behind operator.enable_event_hooks=true if risk of regression exists.

No forked hook logic; single pipeline behavior.

5) Policy scope

No tenant/team hook policy in this PR.

Add only global safety switch:

operator.hooks.enabled = true (default true)

Tenant/team policy is follow-up PR.

6) Observability requirements

Every hook-related structured log event includes:

- correlation/trace id (when available)
- tenant, team
- pack_id, offer_id, offer_key
- stage, contract
- applied action (+ dispatch target when applicable)

7) Subs scope and visibility

This PR keeps subs to discover/register/select only.

No dedicated subs CLI surface in this PR.

Minimal visibility: extend existing doctor output with:

- offer counts by kind
- hook counts by (stage, contract)
- subs counts by contract
