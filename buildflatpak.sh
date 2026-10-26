#!/usr/bin/env bash
# Build the Bindle flatpak and produce bindle-<version>.flatpak next to this
# script. Install the result with: flatpak install --user bindle-<version>.flatpak
#
# With the oyzmo Flatpak site checked out (FLATPAK_SITE) and its signing key
# present (FLATPAK_GNUPG), the build is also published: signed into the site's
# OSTree repo, which GitHub Pages serves at FLATPAK_URL, and the bundle carries
# that address so an installed copy gets its updates from there. The site is
# committed locally; pushing it to GitHub is left to you. Without either, it
# builds a plain bundle as before.
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
FLATPAK_URL="${FLATPAK_URL:-https://flatpak.oyzmo.no/repo/}"
FLATPAK_GNUPG="${FLATPAK_GNUPG:-$HOME/.local/share/oyzmo-flatpak/gnupg}"
FLATPAK_KEY=D7E0928E8C3C9F337EDB33C082C73A44E2040291
# Old builds kept in the published repo besides the newest, for rollback and
# for static deltas from recent versions.
KEEP_DEPTH=3

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m✘ %s\033[0m\n' "$*"; }

if ! command -v flatpak-builder >/dev/null; then
    err "flatpak-builder not found — install it first (e.g. dnf install flatpak-builder)."
    exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
OUT="bindle-${VERSION:-unknown}.flatpak"

publish=0
REPO="$SCRIPT_DIR/repo"
if [ -d "$FLATPAK_SITE/repo/objects" ] && \
        gpg --homedir "$FLATPAK_GNUPG" --list-secret-keys "$FLATPAK_KEY" >/dev/null 2>&1; then
    publish=1
    REPO="$FLATPAK_SITE/repo"
    sign=(--gpg-sign="$FLATPAK_KEY" --gpg-homedir="$FLATPAK_GNUPG")
else
    echo "No $FLATPAK_SITE repo or signing key — building an unpublished bundle."
fi

bold "Building $APP_ID $VERSION…"
if ! flatpak-builder --user --repo="$REPO" --force-clean \
        ${sign[@]+"${sign[@]}"} --subject="Bindle $VERSION" \
        "$SCRIPT_DIR/build-dir" "$MANIFEST"; then
    err "Build failed."
    exit 1
fi

bundle_args=()
if [ "$publish" = 1 ]; then
    bold "Updating the published repo…"
    if ! flatpak build-update-repo "${sign[@]}" --title=oyzmo \
            --homepage=https://oyzmo.no/apps --gpg-import="$FLATPAK_SITE/oyzmo.gpg" \
            --generate-static-deltas --prune --prune-depth="$KEEP_DEPTH" "$REPO"; then
        err "Updating $REPO failed."
        exit 1
    fi
    # The checkout's mount has written 0-byte files before; never publish a
    # repo that doesn't verify.
    if ! ostree fsck --repo="$REPO" >/dev/null; then
        err "$REPO failed ostree fsck — don't push it."
        exit 1
    fi
    bundle_args=(--repo-url="$FLATPAK_URL" --gpg-keys="$FLATPAK_SITE/oyzmo.gpg"
                 --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo)
fi

bold "Creating $OUT…"
mkdir -p "$STAGE_DIR"
if ! flatpak build-bundle ${bundle_args[@]+"${bundle_args[@]}"} \
        "$REPO" "$STAGE_DIR/$OUT" "$APP_ID"; then
    err "Bundling failed."
    exit 1
fi
mkdir -p "$OUT_DIR" || exit 1
cp "$STAGE_DIR/$OUT" "$OUT_DIR/$OUT"
if [ "$(stat -c%s "$OUT_DIR/$OUT")" != "$(stat -c%s "$STAGE_DIR/$OUT")" ]; then
    err "Copy into $OUT_DIR came out wrong (FUSE mount?)."
    err "The good bundle is at $STAGE_DIR/$OUT — copy it manually."
    exit 1
fi
rm -f "$STAGE_DIR/$OUT"

ok "Built $OUT_DIR/$OUT"
echo "  Install it with: flatpak install --user $OUT_DIR/$OUT"
if [ "$publish" = 1 ]; then
    git -C "$FLATPAK_SITE" add -A
    git -C "$FLATPAK_SITE" commit -q -m "Bindle $VERSION" && \
        ok "Published Bindle $VERSION into $FLATPAK_SITE (committed)."
    echo "  Make it live with: git -C $FLATPAK_SITE push"
else
    echo "  (build-dir/ and repo/ are throwaway build artifacts.)"
fi
