#!/usr/bin/env bash
# Bump Bindle's version and record what changed.
#
#   ./bump.sh patch "Fixed a crash when deleting the last lines"
#   ./bump.sh minor "Changelog in the About box" "Bump script"
#
# patch for fixes, minor for features, major for breaking changes. Each note is
# one short line; several notes become a list.
#
# One entry per release, not per change: while the current version has not
# been distributed yet (no dist/bindle-<version>.flatpak), a bump adds its
# notes to that version's entry instead of making a new one. The version only
# moves when the change is bigger than what the entry already is relative to
# the release before it — a minor change on top of an unreleased patch turns
# 1.40.8 into 1.41.0, a fix on top of an unreleased 1.41.0 keeps 1.41.0.
# NEW=1 forces a fresh entry and version anyway.
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
case $part in patch|minor|major) ;; *) usage ;; esac

old="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CARGO" | head -1)"

# Released once the distribution step has built its bundle.
merge=1
[ -e "$SCRIPT_DIR/dist/bindle-$old.flatpak" ] && merge=0
[ "${NEW:-0}" = 1 ] && merge=0

# The metainfo edit (and the version arithmetic) in Python: XML-escaping the
# notes, turning a one-note <p> into a list, re-versioning an entry.
new="$(python3 - "$META" "$old" "$part" "$merge" "$(date +%F)" "$@" <<'EOF'
import html, re, sys

meta, old, part, merge, today, *notes = sys.argv[1:]
merge = merge == "1"
text = open(meta, encoding="utf-8").read()
releases = list(re.finditer(r'    <release version="([^"]+)" date="[^"]*">\n.*?    </release>\n', text, re.S))
LEVELS = ["patch", "minor", "major"]

def parse(v):
    return tuple(int(x) for x in v.split("."))

def bump(v, level):
    a, b, c = parse(v)
    return {"patch": f"{a}.{b}.{c + 1}", "minor": f"{a}.{b + 1}.0", "major": f"{a + 1}.0.0"}[level]

def level_between(base, v):
    a, b = parse(base), parse(v)
    return "major" if b[0] != a[0] else "minor" if b[1] != a[1] else "patch"

def items(block):
    lis = re.findall(r"<li>(.*?)</li>", block, re.S)
    return lis or re.findall(r"<p>(.*?)</p>", block, re.S)

def entry(version, notes_xml):
    body = (f"        <p>{notes_xml[0]}</p>\n" if len(notes_xml) == 1 else
            "        <ul>\n" + "".join(f"          <li>{n}</li>\n" for n in notes_xml) + "        </ul>\n")
    return (f'    <release version="{version}" date="{today}">\n      <description>\n'
            f"{body}      </description>\n    </release>\n")

escaped = [html.escape(n, quote=False) for n in notes]
top = releases[0] if releases else None
if merge and top and top.group(1) == old:
    base = releases[1].group(1) if len(releases) > 1 else None
    if base:
        level = max(part, level_between(base, old), key=LEVELS.index)
        # Never backwards: an entry already past that (1.42.1 over 1.40.7)
        # keeps its version.
        new = max(bump(base, level), old, key=parse)
    else:
        new = old
    text = text[:top.start()] + entry(new, items(top.group(0)) + escaped) + text[top.end():]
else:
    new = bump(old, part)
    if f'<release version="{new}"' in text:
        sys.exit(f"{meta} already has a release {new}.")
    text = text.replace("<releases>\n", "<releases>\n" + entry(new, escaped), 1)
open(meta, "w", encoding="utf-8").write(text)
print(new)
EOF
)" || { err "metainfo was not updated"; exit 1; }

if [ "$new" != "$old" ]; then
    old_re="$(printf '%s' "$old" | sed 's/\./\\./g')"
    sed -i "0,/^version = \".*\"/s//version = \"$new\"/" "$CARGO"
    sed -i "/^name = \"bindle\"$/{n;s/^version = \".*\"/version = \"$new\"/}" "$LOCK"
    sed -i "s/bindle-$old_re/bindle-$new/g" "$README"
fi

# The checkout may sit on a FUSE mount that drops writes — check they landed.
failed=0
grep -q "^version = \"$new\"" "$CARGO" || { err "Cargo.toml was not updated"; failed=1; }
grep -q "^version = \"$new\"" "$LOCK" || { err "Cargo.lock was not updated"; failed=1; }
grep -q "<release version=\"$new\"" "$META" || { err "metainfo was not updated"; failed=1; }
grep -q "bindle-$new" "$README" || { err "README.md was not updated"; failed=1; }
[ "$failed" = 0 ] || exit 1

if [ "$new" = "$old" ]; then
    ok "Bindle $old (unreleased): notes added"
elif [ "$merge" = 1 ]; then
    ok "Bindle $old → $new (unreleased entry updated)"
else
    ok "Bindle $old → $new"
fi
