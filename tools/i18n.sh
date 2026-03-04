#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

EN_PATH="${EN_PATH:-i18n/operator_cli/en.json}"
AUTH_MODE="${AUTH_MODE:-auto}"
LOCALE="${LOCALE:-en}"
BATCH_SIZE="${BATCH_SIZE:-64}"
LANGS="${LANGS:-all}"

usage() {
  cat <<'EOF'
Usage: tools/i18n.sh [translate|validate|status|all]

Sync and verify operator CLI translations using greentic-i18n-translator.

Environment overrides:
  EN_PATH=...           English source file (default: i18n/operator_cli/en.json)
  AUTH_MODE=...         Translator auth mode for translate (default: auto)
  LOCALE=...            Translator CLI locale output (default: en)
  CODEX_HOME=...        Optional codex home path for translator
  BATCH_SIZE=...        Translate batch size (default: 64; larger batches reduce cost)
  LANGS=...             Target languages (default: all, e.g. nl,fr,de)
  MAX_RETRIES=...       Translate max retries (default: translator default)
  GLOSSARY=...          Optional glossary JSON path
  API_KEY_STDIN=1       Pass --api-key-stdin to translator
  OVERWRITE_MANUAL=1    Pass --overwrite-manual to translator
  CACHE_DIR=...         Optional translator cache dir

Examples:
  tools/i18n.sh all
  AUTH_MODE=api-key tools/i18n.sh translate
  EN_PATH=i18n/operator_cli/en.json tools/i18n.sh validate
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

MODE="${1:-all}"

if [[ ! -f "$EN_PATH" ]]; then
  echo "missing EN_PATH file: $EN_PATH" >&2
  exit 1
fi

TRANSLATOR_CMD=()
if command -v greentic-i18n-translator >/dev/null 2>&1; then
  TRANSLATOR_CMD=(greentic-i18n-translator)
elif [[ -f ../greentic-i18n/Cargo.toml ]]; then
  TRANSLATOR_CMD=(cargo run --manifest-path ../greentic-i18n/Cargo.toml -p greentic-i18n-translator --)
else
  echo "greentic-i18n-translator not found on PATH and ../greentic-i18n is unavailable." >&2
  echo "Install the binary or clone ../greentic-i18n." >&2
  exit 1
fi

run_translator() {
  local subcmd="$1"
  shift
  "${TRANSLATOR_CMD[@]}" --locale "$LOCALE" "$subcmd" "$@"
}

translate_args() {
  local args=("--langs" "$LANGS" "--en" "$EN_PATH" "--auth-mode" "$AUTH_MODE" "--batch-size" "$BATCH_SIZE")
  if [[ -n "${CODEX_HOME:-}" ]]; then
    args+=("--codex-home" "$CODEX_HOME")
  fi
  if [[ -n "${MAX_RETRIES:-}" ]]; then
    args+=("--max-retries" "$MAX_RETRIES")
  fi
  if [[ -n "${GLOSSARY:-}" ]]; then
    args+=("--glossary" "$GLOSSARY")
  fi
  if [[ "${API_KEY_STDIN:-0}" == "1" ]]; then
    args+=("--api-key-stdin")
  fi
  if [[ "${OVERWRITE_MANUAL:-0}" == "1" ]]; then
    args+=("--overwrite-manual")
  fi
  if [[ -n "${CACHE_DIR:-}" ]]; then
    args+=("--cache-dir" "$CACHE_DIR")
  fi
  printf '%s\0' "${args[@]}"
}

run_translate() {
  echo "==> status precheck: $EN_PATH"
  if run_translator status --langs "$LANGS" --en "$EN_PATH"; then
    echo "==> translate: skipped (no missing/stale keys)"
    return 0
  fi
  echo "==> translate: $EN_PATH"
  mapfile -d '' -t args < <(translate_args)
  run_translator translate "${args[@]}"
}

run_validate() {
  echo "==> validate: $EN_PATH"
  run_translator validate --langs "$LANGS" --en "$EN_PATH"
}

run_status() {
  echo "==> status: $EN_PATH"
  run_translator status --langs "$LANGS" --en "$EN_PATH"
}

case "$MODE" in
  translate)
    run_translate
    ;;
  validate)
    run_validate
    ;;
  status)
    run_status
    ;;
  all)
    run_translate
    run_validate
    run_status
    ;;
  *)
    echo "Unknown mode: $MODE" >&2
    usage
    exit 2
    ;;
esac
