# PR-OP-03 Remove deprecated dev tooling and dev-mode plumbing

## Title
Remove unreachable `dev` tooling and `dev_mode` plumbing; resolve binaries via config/env/PATH only.

## Problem
The operator still carried legacy `dev` command plumbing (`dev_mode`, `dev_detect`, settings persistence), even though the active CLI/demo flow no longer uses it. This created maintenance overhead and path-resolution surprises.

## Goal
Refactor the operator to keep only active CLI/demo code paths:
- Remove unreachable `dev` command infrastructure.
- Remove `dev_mode` and `dev_detect` modules/settings that are no longer consumed.
- Keep binary resolution deterministic via:
  1. explicit config override (`binaries.<name>`),
  2. env override (`GREENTIC_OPERATOR_BINARY_<NAME>`),
  3. local/bin/target fallbacks,
  4. `PATH`.

## Scope (implemented)
### 1) CLI cleanup
- Removed `Command::Dev` and all dev-only parser/dispatch branches.
- Removed `src/cli/dev_mode_cmd.rs`.
- Removed stale dev-only argument types/impl blocks from `src/cli.rs`.

### 2) Module cleanup
- Removed modules:
  - `src/dev_mode.rs`
  - `src/dev_detect.rs`
  - `src/settings.rs`
- Removed module exports from `src/lib.rs`.
- Removed `OperatorConfig.dev` from `src/config.rs`.
- Updated default project scaffold (`src/project/layout.rs`) to no longer emit `dev:` config.

### 3) Runtime and resolver simplification
- `src/bin_resolver.rs` no longer accepts/uses dev mappings.
- `ResolveCtx` now contains only:
  - `config_dir`
  - `explicit_path`
- `demo` runtime/provider paths no longer pass dev settings:
  - `src/demo/runtime.rs`
  - `src/providers.rs`
- Resolver still supports `binaries.<name>`, `GREENTIC_OPERATOR_BINARY_<NAME>`, local fallbacks, and `PATH`.

### 4) Test cleanup and updates
- Removed obsolete dev-only tests:
  - `tests/bin_resolver_dev_mode.rs`
  - `tests/bin_resolver_global_dev_mode.rs`
  - `tests/bin_resolver_hybrid.rs`
  - `tests/dev_detect.rs`
  - `tests/settings_persistence.rs`
  - `tests/dev_events_up.rs`
- Updated callsites impacted by signature changes:
  - `tests/demo_up_smoke.rs`
  - `tests/demo_events_up.rs`
  - `tests/provider_setup_smoke.rs`
  - `tests/secrets_setup_integration.rs`

### Security posture after cleanup
- No implicit trust in source-tree `target/` paths for demo runtime commands.
- No hidden mode switches via `dev.repo_map` affecting demo behavior.
- Reduced attack surface from stale dev-only execution paths.

## Acceptance Criteria
- `greentic-operator --help` contains no `dev` command group.
- `cargo build --bin greentic-operator` passes.
- `cargo test -q` passes.
- Demo doctor path passes:
  - `greentic-operator demo doctor --bundle <bundle>`

## Security/Reliability rationale
- Removes accidental dependence on local source-tree build artifacts for demo operations.
- Avoids hidden execution-path differences between developers and CI/users.
- Reduces attack surface from untrusted dev-root path assumptions in demo mode.

## Backward compatibility
- Breaking only for users who passed dev-mode flags to `demo` commands.
- Functional behavior for normal users improves (PATH-installed binaries now work consistently).

## Validation
- `cargo build --bin greentic-operator` ✅
- `cargo test -q` ✅
- `cargo run --bin greentic-operator -- demo doctor --bundle ../test/wiz-test` ✅
