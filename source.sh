#!/usr/bin/env bash
# Package the Bindle sources for release: builds bindle-<version>-src.zip next
# to this script, containing everything needed to build the app plus the GPL
# LICENSE and the THIRD-PARTY.md attribution notices (both mandatory — the GPL
# requires the licence to travel with every copy of the source).
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Where the finished zip lands. Defaults to next to this script; bindle.sh's
# "Make distribution" points it at dist/ so the release files sit together.
OUT_DIR="${BINDLE_OUT_DIR:-$SCRIPT_DIR}"
# The checkout may live on a FUSE mount that silently truncates direct writes
# (0-byte files) — zip to a real filesystem first, then copy in and verify.
STAGE_ROOT="$HOME/.cache/bindle/source"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m✘ %s\033[0m\n' "$*"; }

# What gets published. Deliberately mirrors .gitignore: no target/, no
# build-dir/, no .flatpak bundles, no dev scratch, no CLAUDE.md, no myinput/.
ITEMS=(
    LICENSE
    THIRD-PARTY.md
    README.md
    Cargo.toml
    Cargo.lock
    .gitignore
    bindle.sh
    source.sh
    bump.sh
    no.oyzmo.Bindle.yml
    screenshot.png
    src
    tests
    examples
    data
)
# Missing any of these means the zip would ship without its licence terms.
REQUIRED=(LICENSE THIRD-PARTY.md)

if ! command -v zip >/dev/null; then
    err "zip not found — install it first (e.g. dnf install zip)."
    exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
NAME="bindle-${VERSION:-unknown}"
OUT="$NAME-src.zip"

for item in "${REQUIRED[@]}"; do
    if [ ! -s "$SCRIPT_DIR/$item" ]; then
        err "$item is missing or empty — refusing to build a source zip without it."
        exit 1
    fi
done

bold "Packaging $NAME sources…"
rm -rf "$STAGE_ROOT"
mkdir -p "$STAGE_ROOT/$NAME" || exit 1

missing=0
for item in "${ITEMS[@]}"; do
    if [ -e "$SCRIPT_DIR/$item" ]; then
        cp -r "$SCRIPT_DIR/$item" "$STAGE_ROOT/$NAME/" || exit 1
    else
        err "skipped (not found): $item"
        missing=1
    fi
done

if ! (cd "$STAGE_ROOT" && zip -qr "$OUT" "$NAME"); then
    err "zip failed."
    exit 1
fi

mkdir -p "$OUT_DIR" || exit 1
cp "$STAGE_ROOT/$OUT" "$OUT_DIR/$OUT" || exit 1
if [ "$(stat -c%s "$OUT_DIR/$OUT")" != "$(stat -c%s "$STAGE_ROOT/$OUT")" ]; then
    err "Copy into $OUT_DIR came out wrong (FUSE mount?)."
    err "The good zip is at $STAGE_ROOT/$OUT — copy it manually."
    exit 1
fi
rm -rf "$STAGE_ROOT"

ok "Built $OUT_DIR/$OUT ($(du -h "$OUT_DIR/$OUT" | cut -f1))"
echo "  Unpacks to $NAME/ — build it with: cd $NAME && ./bindle.sh"
[ "$missing" = 0 ] || err "Some items were missing (see above); check before releasing."
