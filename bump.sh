#!/usr/bin/env bash
# Bump Bindle's version and record what changed.
#
#   ./bump.sh patch "Fixed a crash when deleting the last lines"
#   ./bump.sh minor "Changelog in the About box" "Bump script"
#
# patch for fixes, minor for features, major for breaking changes. Each note is
# one short line; several notes become a list.
#
# Cargo.toml is the single source of the version — the app (CARGO_PKG_VERSION),
# buildflatpak.sh and source.sh all read it. The notes go into the metainfo
# <releases> list, which GNOME Software and the About box's "What's New" page
# both show. Cargo.lock and the README's unzip example are kept in step.
set -eu

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CARGO="$SCRIPT_DIR/Cargo.toml"
LOCK="$SCRIPT_DIR/Cargo.lock"
META="$SCRIPT_DIR/data/no.oyzmo.Bindle.metainfo.xml"
README="$SCRIPT_DIR/README.md"

ok()  { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err() { printf '\033[31m✘ %s\033[0m\n' "$*" >&2; }
usage() {
    echo "usage: $0 patch|minor|major \"note\" [\"note\"…]" >&2
    exit 2
}

[ $# -ge 2 ] || usage
part=$1
shift

old="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO" | head -1)"
IFS=. read -r major minor patch <<<"$old"
case $part in
    patch) patch=$((patch + 1)) ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    major) major=$((major + 1)); minor=0; patch=0 ;;
    *) usage ;;
esac
new="$major.$minor.$patch"

if grep -q "<release version=\"$new\"" "$META"; then
    err "$META already has a release $new."
    exit 1
fi

# Notes land inside XML (and from there in GTK markup).
esc() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }

block="$(mktemp)"
trap 'rm -f "$block"' EXIT
{
    printf '    <release version="%s" date="%s">\n' "$new" "$(date +%F)"
    printf '      <description>\n'
    if [ $# -eq 1 ]; then
        printf '        <p>%s</p>\n' "$(esc "$1")"
    else
        printf '        <ul>\n'
        for note in "$@"; do
            printf '          <li>%s</li>\n' "$(esc "$note")"
        done
        printf '        </ul>\n'
    fi
    printf '      </description>\n'
    printf '    </release>\n'
} >"$block"

old_re="$(printf '%s' "$old" | sed 's/\./\\./g')"
sed -i "0,/^version = \".*\"/s//version = \"$new\"/" "$CARGO"
sed -i "/^name = \"bindle\"$/{n;s/^version = \".*\"/version = \"$new\"/}" "$LOCK"
sed -i "/<releases>/r $block" "$META"
sed -i "s/bindle-$old_re/bindle-$new/g" "$README"

# The checkout may sit on a FUSE mount that drops writes — check they landed.
failed=0
grep -q "^version = \"$new\"" "$CARGO" || { err "Cargo.toml was not updated"; failed=1; }
grep -q "^version = \"$new\"" "$LOCK" || { err "Cargo.lock was not updated"; failed=1; }
grep -q "<release version=\"$new\"" "$META" || { err "metainfo was not updated"; failed=1; }
grep -q "bindle-$new" "$README" || { err "README.md was not updated"; failed=1; }
[ "$failed" = 0 ] || exit 1

ok "Bindle $old → $new"
