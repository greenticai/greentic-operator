#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=""
OUT_DIR=""
VERSION=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target)
      TARGET="$2"
      shift 2
      ;;
    --version)
      VERSION="$2"
      shift 2
      ;;
    --out)
      OUT_DIR="$2"
      shift 2
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -z "$OUT_DIR" ]]; then
  echo "--out is required" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

HOST_TARGET="$(rustc -vV | awk '/^host:/ {print $2}')"
if [[ -z "$TARGET" ]]; then
  TARGET="$HOST_TARGET"
fi

pushd "$ROOT_DIR" >/dev/null

if [[ "$TARGET" != "$HOST_TARGET" ]]; then
  rustup target add "$TARGET"
  if [[ "${USE_CROSS:-0}" == "1" ]] && command -v cross >/dev/null 2>&1; then
    cross build --release --target "$TARGET"
  else
    cargo build --locked --release --target "$TARGET"
  fi
  BIN_DIR="target/$TARGET/release"
else
  cargo build --locked --release
  BIN_DIR="target/release"
fi

BIN_NAME="greentic-operator"
if [[ "$TARGET" == *windows* ]]; then
  BIN_NAME="${BIN_NAME}.exe"
fi

if [[ -z "$VERSION" ]]; then
  echo "--version is required for binstall packaging" >&2
  exit 1
fi
ARCHIVE_PREFIX="greentic-operator-${TARGET}-v${VERSION}"
STAGING_DIR="$(mktemp -d)"
STAGING_ROOT="$STAGING_DIR/$ARCHIVE_PREFIX"
mkdir -p "$STAGING_ROOT"
cp "$BIN_DIR/$BIN_NAME" "$STAGING_ROOT/$BIN_NAME"

if [[ "$TARGET" == *windows* ]]; then
  ARCHIVE="$OUT_DIR/${ARCHIVE_PREFIX}.zip"
  if command -v 7z >/dev/null 2>&1; then
    pushd "$STAGING_DIR" >/dev/null
    7z a -tzip "$ARCHIVE" "$ARCHIVE_PREFIX/$BIN_NAME" >/dev/null
    popd >/dev/null
  else
    echo "7z is required to package Windows artifacts." >&2
    exit 1
  fi
else
  ARCHIVE="$OUT_DIR/${ARCHIVE_PREFIX}.tgz"
  tar -C "$STAGING_DIR" -czf "$ARCHIVE" "$ARCHIVE_PREFIX"
fi

popd >/dev/null

rm -rf "$STAGING_DIR"

echo "$ARCHIVE"
