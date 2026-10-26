#!/usr/bin/env bash
# Bindle helper menu: install build tools, compile, install, uninstall,
# package a distribution, clean up build files.
set -u

APP_ID="no.oyzmo.Bindle"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Build outside the source tree (works around network/FUSE-mounted checkouts).
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/bindle/build}"

BIN_DIR="$HOME/.local/bin"
DESKTOP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
SYMBOLIC_DIR="$HOME/.local/share/icons/hicolor/symbolic/apps"
# Same layout the Flatpak uses (/app/share/licenses/$FLATPAK_ID): the GPL
# licence and the third-party attribution notices must travel with the app.
LICENSE_DIR="$HOME/.local/share/licenses/$APP_ID"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[32m✔ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m✘ %s\033[0m\n' "$*"; }

install_tools() {
    bold "Installing build tools…"
    if command -v dnf >/dev/null; then
        sudo dnf install -y cargo rust gcc pkgconf-pkg-config gtk4-devel libadwaita-devel
    elif command -v apt >/dev/null; then
        sudo apt update && sudo apt install -y cargo rustc gcc pkg-config libgtk-4-dev libadwaita-1-dev
    elif command -v pacman >/dev/null; then
        sudo pacman -S --needed rust gcc pkgconf gtk4 libadwaita
    elif command -v zypper >/dev/null; then
        sudo zypper install -y cargo rust gcc pkg-config gtk4-devel libadwaita-devel
    else
        err "No supported package manager found (dnf/apt/pacman/zypper)."
        err "Install manually: rust/cargo, gcc, pkg-config, GTK4 + Libadwaita dev headers."
        return 1
    fi
    ok "Build tools installed."
}

compile_app() {
    bold "Compiling Bindle (release)…"
    if ! command -v cargo >/dev/null; then
        err "cargo not found — run option 1 first."
        return 1
    fi
    if (cd "$SCRIPT_DIR" && cargo build --release); then
        ok "Built $CARGO_TARGET_DIR/release/bindle"
    else
        err "Build failed."
        return 1
    fi
}

install_app() {
    local binary="$CARGO_TARGET_DIR/release/bindle"
    # Always (re)compile so option 3 also updates an existing installation;
    # cargo is incremental, so this is a no-op when nothing changed.
    compile_app || return 1
    bold "Installing to your GNOME desktop…"
    mkdir -p "$BIN_DIR" "$DESKTOP_DIR" "$ICON_DIR" "$SYMBOLIC_DIR" "$LICENSE_DIR"
    install -m 755 "$binary" "$BIN_DIR/bindle"
    install -m 644 "$SCRIPT_DIR/LICENSE" "$LICENSE_DIR/LICENSE"
    install -m 644 "$SCRIPT_DIR/THIRD-PARTY.md" "$LICENSE_DIR/THIRD-PARTY.md"
    install -m 644 "$SCRIPT_DIR/data/$APP_ID.svg" "$ICON_DIR/$APP_ID.svg"
    install -m 644 "$SCRIPT_DIR/data/$APP_ID-symbolic.svg" "$SYMBOLIC_DIR/$APP_ID-symbolic.svg"
    # Use an absolute Exec path so it works even if ~/.local/bin is not in PATH.
    sed "s|^Exec=bindle|Exec=$BIN_DIR/bindle|" \
        "$SCRIPT_DIR/data/$APP_ID.desktop" > "$DESKTOP_DIR/$APP_ID.desktop"
    chmod 644 "$DESKTOP_DIR/$APP_ID.desktop"
    command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null
    ok "Installed. Find “Bindle” in your app grid (log out/in if it does not appear)."
    echo "  GPL licence and third-party notices: $LICENSE_DIR"
}

templates_dir() {
    xdg-user-dir TEMPLATES 2>/dev/null || echo "$HOME/Templates"
}

install_template() {
    local dir
    dir="$(templates_dir)"
    bold "Installing file-manager template…"
    mkdir -p "$dir"
    install -m 644 "$SCRIPT_DIR/data/Markdown file.md" "$dir/Markdown file.md"
    ok "Right-click in Files → New Document → “Markdown file”."
}

remove_template() {
    rm -f "$(templates_dir)/Markdown file.md"
    ok "Template removed."
}

# Everything a release consists of, in one folder: the source zip (source.sh)
# and the flatpak bundle (buildflatpak.sh). Both scripts honour BINDLE_OUT_DIR,
# so they write straight into dist/ instead of littering the checkout root.
make_distribution() {
    local dist="$SCRIPT_DIR/dist"
    local version failed=0
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1)"
    bold "Making distribution for Bindle ${version:-unknown} → dist/"
    mkdir -p "$dist" || return 1

    if [ -x "$SCRIPT_DIR/source.sh" ]; then
        BINDLE_OUT_DIR="$dist" "$SCRIPT_DIR/source.sh" || failed=1
    else
        err "source.sh not found — skipping the source zip."
        failed=1
    fi

    # buildflatpak.sh is dev tooling and is not part of the source zip, so a
    # checkout unpacked from a release simply has no flatpak step.
    if [ -x "$SCRIPT_DIR/buildflatpak.sh" ]; then
        BINDLE_OUT_DIR="$dist" "$SCRIPT_DIR/buildflatpak.sh" || failed=1
    else
        err "buildflatpak.sh not found — skipping the flatpak bundle."
        failed=1
    fi

    echo
    bold "dist/ now holds:"
    ls -lh "$dist" | tail -n +2 | sed 's/^/  /'
    if [ "$failed" = 0 ]; then
        ok "Distribution ready in $dist"
    else
        err "Distribution incomplete (see above)."
        return 1
    fi
}

# Throw-away build output only. Nothing here is a release artifact: dist/, the
# installed app and ~/.config/bindle are all left alone.
cleanup_build() {
    local targets=(
        "$SCRIPT_DIR/build-dir"          # flatpak-builder work tree
        "$SCRIPT_DIR/repo"               # local ostree repo the bundle is cut from
        "$SCRIPT_DIR/.flatpak-builder"   # flatpak-builder download/ccache
        "$SCRIPT_DIR/target"             # cargo, when built without CARGO_TARGET_DIR
        "$CARGO_TARGET_DIR"              # cargo, the usual location on this machine
        "$HOME/.cache/bindle/source"     # source.sh staging (left behind on failure)
        "$HOME/.cache/bindle/gtkinit"    # cargo target dir used by GTK test runs
    )
    local present=() t
    for t in "${targets[@]}"; do
        [ -e "$t" ] && present+=("$t")
    done
    if [ "${#present[@]}" = 0 ]; then
        ok "Nothing to clean."
        return 0
    fi

    bold "These build directories will be deleted:"
    for t in "${present[@]}"; do
        printf '  %s  (%s)\n' "$t" "$(du -sh "$t" 2>/dev/null | cut -f1)"
    done
    echo "  (dist/, the installed app and ~/.config/bindle are not touched.)"
    read -rp "Delete them? [y/N] " confirm
    case "$confirm" in
        y|Y)
            for t in "${present[@]}"; do
                rm -rf "$t" || err "could not remove $t"
            done
            ok "Build files removed. The next compile starts from scratch."
            ;;
        *) echo "Cancelled." ;;
    esac
}

# Maintainer only: dev/ is not in the source zip, so the menu shows this step
# only in a checkout that has both scripts. GitHub gets the filtered copy of
# master first (the release is tagged there), then the dist/ files; the release
# script shows the notes and asks before publishing.
github_scripts() {
    [ -x "$SCRIPT_DIR/dev/push-github.sh" ] && [ -x "$SCRIPT_DIR/dev/release-github.sh" ]
}

release_github() {
    bold "Pushing master to GitHub…"
    "$SCRIPT_DIR/dev/push-github.sh" || { err "Push failed — nothing released."; return 1; }
    "$SCRIPT_DIR/dev/release-github.sh"
}

current_version() {
    sed -n 's/^version = "\(.*\)"/\1/p' "$SCRIPT_DIR/Cargo.toml" | head -1
}

# bump.sh plus a commit of just the files it touches, so the release steps
# after it (which clone master) see the new version.
bump_version() {
    local level notes=() note
    echo "  Now at $(current_version)."
    read -rp "Bump which part? [p]atch (fixes) / [m]inor (features) / ma[j]or: " level
    case "$level" in
        p|P|patch) level=patch ;;
        m|M|minor) level=minor ;;
        j|J|major) level=major ;;
        *) echo "  Cancelled."; return 0 ;;
    esac
    echo "  What changed? One short line per note; an empty line ends."
    while read -rp "  note: " note && [ -n "$note" ]; do
        notes+=("$note")
    done
    [ ${#notes[@]} -gt 0 ] || { err "At least one note is needed."; return 1; }
    "$SCRIPT_DIR/bump.sh" "$level" "${notes[@]}" || return 1
    git -C "$SCRIPT_DIR" commit -q -m "Bindle $(current_version)" -- \
        Cargo.toml Cargo.lock README.md data/$APP_ID.metainfo.xml \
        && ok "Committed Bindle $(current_version)."
}

# The oyzmo Flatpak site that buildflatpak.sh publishes into; pushing it to
# GitHub is what makes the update appear in GNOME Software.
FLATPAK_SITE="${FLATPAK_SITE:-/mnt/mycode/flatpak}"

publish_flatpak() {
    if [ ! -d "$FLATPAK_SITE/.git" ]; then
        err "No Flatpak site at $FLATPAK_SITE."
        return 1
    fi
    git -C "$FLATPAK_SITE" fetch -q origin 2>/dev/null
    local pending
    pending="$(git -C "$FLATPAK_SITE" log --format='  %s' origin/master..master 2>/dev/null)"
    if [ -z "$pending" ]; then
        ok "flatpak.oyzmo.no is up to date — nothing new to publish."
        return 0
    fi
    echo "  Not yet on flatpak.oyzmo.no:"
    echo "$pending"
    read -rp "Publish now? [y/N] " answer
    [[ "$answer" == [yY]* ]] || { echo "  Cancelled."; return 0; }
    git -C "$FLATPAK_SITE" push -q origin master || { err "Push to GitHub failed."; return 1; }
    git -C "$FLATPAK_SITE" push -q nas master || err "Backup push to the NAS failed (GitHub is fine)."
    ok "Published. GNOME Software shows the update within a few minutes."
}

# Everything after the bump: dist/ (which also publishes into the Flatpak
# site), NAS backup, flatpak.oyzmo.no, GitHub release.
full_release() {
    if ! git -C "$SCRIPT_DIR" diff --quiet HEAD; then
        err "There are uncommitted changes — commit them first, a release is built from master."
        return 1
    fi
    bold "Full release of Bindle $(current_version)"
    make_distribution || { err "Stopped: the distribution failed."; return 1; }
    bold "Backing up to the NAS…"
    git -C "$SCRIPT_DIR" push -q nas master || err "NAS push failed — continuing."
    publish_flatpak || err "flatpak.oyzmo.no was not updated."
    if github_scripts; then
        release_github || err "The GitHub release failed (see above)."
    fi
}

# The signing key exists only on this machine; if it is lost, no update can be
# signed again and everyone has to reinstall. This keeps an exported copy in a
# private repo on the NAS. Restore with: gpg --homedir <dir> --import <file>
FLATPAK_KEYS="$HOME/.local/share/oyzmo-flatpak"
backup_key() {
    local gnupg="$FLATPAK_KEYS/gnupg" dir="$FLATPAK_KEYS/backup"
    local key=D7E0928E8C3C9F337EDB33C082C73A44E2040291 repo=oyzmo-flatpak-key
    if ! gpg --homedir "$gnupg" --list-secret-keys "$key" >/dev/null 2>&1; then
        err "No signing key in $gnupg."
        return 1
    fi
    mkdir -p "$dir" && chmod 700 "$dir"
    (umask 077; gpg --homedir "$gnupg" --armor --export-secret-keys "$key" \
        > "$dir/oyzmo-flatpak-key.asc") || { err "Export failed."; return 1; }
    cp "$gnupg/openpgp-revocs.d/$key.rev" "$dir/" 2>/dev/null
    printf '%s\n' "Signing key for flatpak.oyzmo.no. Restore on a new machine with:" \
        "  mkdir -p -m 700 $gnupg && gpg --homedir $gnupg --import oyzmo-flatpak-key.asc" \
        > "$dir/README"
    if [ ! -d "$dir/.git" ]; then
        git -C "$dir" init -q -b master
        git -C "$dir" config user.name oyzmo
        git -C "$dir" config user.email 74640760+oyzmo@users.noreply.github.com
        git -C "$dir" remote add nas "softserve:$repo.git"
        # Private before the first push, so the key is never readable.
        ssh softserve repo create "$repo" --private --hidden 2>/dev/null
    fi
    ssh softserve repo private "$repo" true || { err "Could not make $repo private — not pushing."; return 1; }
    git -C "$dir" add -A
    git -C "$dir" diff --cached --quiet || git -C "$dir" commit -q -m "Signing key backup"
    git -C "$dir" push -q nas master || { err "Push to the NAS failed."; return 1; }
    local check
    check="$(mktemp -d /tmp/keycheck.XXXXXX)"
    if git clone -q "softserve:$repo.git" "$check/r" && \
            gpg --show-keys --with-colons "$check/r/oyzmo-flatpak-key.asc" 2>/dev/null | grep -q "$key"; then
        ok "Key backed up to softserve:$repo.git (private) and verified."
    else
        err "The NAS copy did not verify."
    fi
    rm -rf "$check"
}

uninstall_app() {
    bold "Uninstalling Bindle…"
    rm -f "$BIN_DIR/bindle" "$DESKTOP_DIR/$APP_ID.desktop" "$ICON_DIR/$APP_ID.svg" \
          "$SYMBOLIC_DIR/$APP_ID-symbolic.svg" \
          "$LICENSE_DIR/LICENSE" "$LICENSE_DIR/THIRD-PARTY.md"
    rmdir "$LICENSE_DIR" 2>/dev/null
    command -v update-desktop-database >/dev/null && update-desktop-database "$DESKTOP_DIR" 2>/dev/null
    ok "Removed binary, desktop entry and icon."
    echo "  (Settings in ~/.config/bindle are kept; delete that folder to remove them.)"
}

# Swap an installed Bindle flatpak for the newest bundle in dist/ (or next to
# this script, where buildflatpak.sh puts it by default). GNOME Software only
# offers "Open" for a bundle of an app that is already installed, so a newer
# bundle never replaces the old one by double-clicking. Uninstalling keeps the
# app's data (~/.var/app/$APP_ID).
reinstall_flatpak() {
    if ! command -v flatpak >/dev/null; then
        err "flatpak not found."
        return 1
    fi
    local bundle
    local f
    # Newest by file name — the folder path may hold dashes of its own.
    bundle="$(for f in "$SCRIPT_DIR"/dist/bindle-*.flatpak "$SCRIPT_DIR"/bindle-*.flatpak; do
        [ -f "$f" ] && printf '%s\t%s\n' "$(basename "$f")" "$f"
    done | sort -V | tail -1 | cut -f2-)"
    if [ -z "$bundle" ]; then
        err "No bindle-*.flatpak found in dist/ — make one with option 7 first."
        return 1
    fi
    # Installed per user, system-wide (what Software does) or both; a copy
    # left in the other one would keep shadowing the new install.
    local scope found=() version
    for scope in user system; do
        version="$(flatpak info --"$scope" "$APP_ID" 2>/dev/null | sed -n 's/^ *Version: //p')"
        if flatpak info --"$scope" "$APP_ID" >/dev/null 2>&1; then
            found+=("$scope")
            echo "  Installed ($scope): ${version:-unknown version}"
        fi
    done
    [ ${#found[@]} -eq 0 ] && echo "  Not installed yet."
    echo "  New:  $(basename "$bundle")"
    read -rp "Uninstall the old and install this one? [y/N] " answer
    [[ "$answer" == [yY]* ]] || { echo "  Cancelled."; return 0; }
    for scope in "${found[@]}"; do
        bold "Uninstalling ($scope)…"
        flatpak uninstall --"$scope" -y --noninteractive "$APP_ID" || { err "Uninstall failed."; return 1; }
    done
    # Back where it was; a first install goes per user, no password needed.
    scope="user"
    [ ${#found[@]} -eq 1 ] && scope="${found[0]}"
    bold "Installing $(basename "$bundle") ($scope)…"
    if flatpak install --"$scope" -y --noninteractive "$bundle"; then
        ok "Installed $(flatpak info --"$scope" "$APP_ID" | sed -n 's/^ *Version: //p')."
    else
        err "Install failed."
        return 1
    fi
}

while true; do
    echo
    bold "── Bindle ───────────────────────────────"
    echo "  1) Install tools needed to compile"
    echo "  2) Compile"
    echo "  3) Install on this GNOME desktop"
    echo "  4) Uninstall"
    echo "  5) Install “New Document” template (Files right-click)"
    echo "  6) Remove template"
    echo "  8) Cleanup (delete temporary build files)"
    echo " 10) Replace installed flatpak with the newest one (from dist/)"
    bold "── Release (in this order) ──────────────"
    echo " 11) Bump version (asks for notes, commits)"
    echo " 13) Full release: 7 → NAS → 12 → 9 in one go"
    echo "     or step by step:"
    echo "  7)   Make distribution (source zip + flatpak → dist/ and the Flatpak site)"
    echo " 12)   Publish flatpak updates (flatpak.oyzmo.no → GNOME Software)"
    github_scripts && echo "  9)   Release newest version on GitHub (from dist/)"
    echo " 14) Back up the flatpak signing key to the NAS (after the key changes)"
    echo "  q) Quit"
    read -rp "Choose: " choice
    case "$choice" in
        1) install_tools ;;
        2) compile_app ;;
        3) install_app ;;
        4) uninstall_app ;;
        5) install_template ;;
        6) remove_template ;;
        7) make_distribution ;;
        8) cleanup_build ;;
        9) if github_scripts; then release_github; else err "Unknown choice."; fi ;;
        10) reinstall_flatpak ;;
        11) bump_version ;;
        12) publish_flatpak ;;
        13) full_release ;;
        14) backup_key ;;
        q|Q) exit 0 ;;
        *) err "Unknown choice." ;;
    esac
done
