#!/usr/bin/env bash
# Build the Bindle flatpak and produce bindle-<version>.flatpak next to this
# script. Install the result with: flatpak install --user bindle-<version>.flatpak
#
# With the oyzmo Flatpak site checked out (FLATPAK_SITE) and its signing key
# present, the build is also published there through the site's
# tools/publish.sh: signed into its OSTree repo, which GitHub Pages serves as
# flatpak.oyzmo.no, and the bundle carries that address so an installed copy
# gets its updates from there. The site is committed locally; pushing it live
# is bindle.sh's "Publish flatpak updates". Without either, a plain bundle.
set -u

APP_ID="no.oyzmo.Bindle"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$SCRIPT_DIR/$APP_ID.yml"
# Where the finished bundle lands. Defaults to next to this script; bindle.sh's
# "Make distribution" points it at dist/ so the release files sit together.
OUT_DIR="${BINDLE_OUT_DIR:-$SCRIPT_DIR}"
# The checkout may live on a FUSE mount that silently truncates direct
# writes (0-byte files) — bundle to a real filesystem first, then copy in.
STAGE_DIR="$HOME/.cache/bindle"
FLATPAK_SITE="${FLATPAK_SITE:-/mnt/mycode/flatpak}"
FLATPAK_GNUPG="${FLATPAK_GNUPG:-$HOME/.local/share/oyzmo-flatpak/gnupg}"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m✘ %s\033[0m\n' "$*"; }

if ! command -v flatpak-builder >/dev/null; then
    err "flatpak-builder not found — install it first (e.g. dnf install flatpak-builder)."
    exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
OUT="bindle-${VERSION:-unknown}.flatpak"

bold "Building $APP_ID $VERSION…"
if ! flatpak-builder --user --repo="$SCRIPT_DIR/repo" --force-clean \
        "$SCRIPT_DIR/build-dir" "$MANIFEST"; then
    err "Build failed."
    exit 1
fi

mkdir -p "$OUT_DIR" || exit 1
if [ -x "$FLATPAK_SITE/tools/publish.sh" ] && [ -d "$FLATPAK_GNUPG" ]; then
    # publish.sh bundles via a real filesystem and checks the copy itself.
    "$FLATPAK_SITE/tools/publish.sh" "$SCRIPT_DIR/repo" "$APP_ID" Bindle "$VERSION" \
        "$OUT_DIR/$OUT" || exit 1
    exit 0
fi

echo "No $FLATPAK_SITE or signing key — building an unpublished bundle."
bold "Creating $OUT…"
mkdir -p "$STAGE_DIR"
if ! flatpak build-bundle "$SCRIPT_DIR/repo" "$STAGE_DIR/$OUT" "$APP_ID"; then
    err "Bundling failed."
    exit 1
fi
cp "$STAGE_DIR/$OUT" "$OUT_DIR/$OUT"
if [ "$(stat -c%s "$OUT_DIR/$OUT")" != "$(stat -c%s "$STAGE_DIR/$OUT")" ]; then
    err "Copy into $OUT_DIR came out wrong (FUSE mount?)."
    err "The good bundle is at $STAGE_DIR/$OUT — copy it manually."
    exit 1
fi
rm -f "$STAGE_DIR/$OUT"

ok "Built $OUT_DIR/$OUT"
echo "  Install it with: flatpak install --user $OUT_DIR/$OUT"
echo "  (build-dir/ and repo/ are throwaway build artifacts.)"
