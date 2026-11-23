//! Backup copies: documents the user flags in the Files panel are copied to
//! one backup folder after every save — the latest copy only, one-way.
//! GTK-free and unit-tested; `ui.rs` decides when, runs `copy` on a worker
//! thread (the folder may sit on a network share that hangs) and asks the
//! user when `copy` finds the copy was changed by someone else.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::settings::path_json;

/// One flagged document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The document: a file, or a `.textbundle` directory.
    #[serde(with = "path_json")]
    pub path: PathBuf,
    /// Its name in the backup folder, chosen once when it was flagged
    /// (`name_for`) and never changed, so a copy never moves.
    #[serde(with = "path_json")]
    pub name: PathBuf,
    /// The copy as Bindle last wrote it: seconds, nanoseconds, size. A copy
    /// that no longer matches was changed by someone else.
    #[serde(default)]
    pub stamp: Option<(u64, u32, u64)>,
    /// The last copy failed (share offline): retried on the next save, at
    /// quit and at the next launch.
    #[serde(default)]
    pub pending: bool,
    /// The referenced files (images, attachments) copied along with it, as
    /// Bindle last wrote them: what lets a later copy tell its own files
    /// from another document's or someone else's at the same path.
    #[serde(default)]
    pub assets: Vec<AssetCopy>,
    /// A copy failed after it had started writing (the drive went away):
    /// whatever is at the target now is Bindle's own half-written copy, so
    /// the next copy replaces it without asking.
    #[serde(default)]
    pub interrupted: bool,
}

/// A referenced file copied beside a document's copy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssetCopy {
    /// Relative to the backup folder (and to the document).
    #[serde(with = "path_json")]
    pub path: PathBuf,
    pub stamp: Stamp,
}

/// `Entry::stamp`.
pub type Stamp = (u64, u32, u64);

/// Copies run one at a time, whoever starts them: the copy started by a
/// save and the last try at quit must not write the same temp file at once.
static COPY_LOCK: Mutex<()> = Mutex::new(());

/// Everything `copy` needs, taken from an `Entry` on the main thread.
#[derive(Clone, Debug)]
pub struct Job {
    pub src: PathBuf,
    pub folder: PathBuf,
    pub name: PathBuf,
    pub last: Option<Stamp>,
    pub assets: Vec<AssetCopy>,
    /// The names of every flagged document's copy: a reference that would
    /// land on one of them is never copied.
    pub reserved: Vec<PathBuf>,
    /// Replace the copy even if it was changed elsewhere.
    pub force: bool,
    /// `Entry::interrupted`.
    pub interrupted: bool,
    /// The mount point the backup folder sat on when it was chosen
    /// (`mount_point`). Found on another one now, the drive is not mounted
    /// and the folder is just the empty directory underneath.
    pub mount: Option<PathBuf>,
}

impl Job {
    pub fn for_entry(entry: &Entry, folder: &Path, mount: Option<&Path>, all: &[Entry], force: bool) -> Job {
        Job {
            interrupted: entry.interrupted,
            mount: mount.map(Path::to_path_buf),
            src: entry.path.clone(),
            folder: folder.to_path_buf(),
            name: entry.name.clone(),
            last: entry.stamp,
            assets: entry.assets.clone(),
            reserved: all.iter().map(|e| e.name.clone()).collect(),
            force,
        }
    }
}

fn stamp_of(mtime: SystemTime, size: u64) -> Stamp {
    let d = mtime.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    (d.as_secs(), d.subsec_nanos(), size)
}

/// The name a newly flagged document gets in the backup folder: its own
/// file name, unless another flagged document already has that — then the
/// parent folder's name goes in brackets (`notes (Work).md`), then two
/// levels (`notes (a-x).md`), and a number as the last resort. Compared
/// case-insensitively, since the backup folder may be on SMB or FAT.
pub fn name_for(path: &Path, taken: &[&Path]) -> PathBuf {
    let free = |name: &OsStr| {
        let lower = name.to_string_lossy().to_lowercase();
        !taken.iter().any(|t| t.as_os_str().to_string_lossy().to_lowercase() == lower)
    };
    let file_name = path.file_name().map(OsStr::to_os_string).unwrap_or_else(|| "document".into());
    if free(&file_name) {
        return file_name.into();
    }
    let (stem, ext) = split_ext(&file_name);
    let with = |tag: &str| {
        let mut name = stem.clone();
        name.push(format!(" ({tag})"));
        if let Some(ext) = &ext {
            name.push(".");
            name.push(ext);
        }
        name
    };
    let parents: Vec<String> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let mut tag = String::new();
    for parent in parents.iter().rev().take(2) {
        tag = if tag.is_empty() { parent.clone() } else { format!("{parent}-{tag}") };
        let name = with(&tag);
        if free(&name) {
            return name.into();
        }
    }
    (2..).map(|n| with(&n.to_string())).find(|n| free(n)).expect("unbounded").into()
}

fn split_ext(name: &OsStr) -> (OsString, Option<OsString>) {
    let p = Path::new(name);
    match (p.file_stem(), p.extension()) {
        (Some(stem), Some(ext)) => (stem.to_os_string(), Some(ext.to_os_string())),
        _ => (name.to_os_string(), None),
    }
}

/// What `copy` did.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Written.
    Copied(Copied),
    /// Not written: something is already at the target that Bindle did not
    /// write last (edited elsewhere, or an unrelated file of the same name).
    /// Run again with `force` to replace it.
    Changed,
    /// The document itself is gone; nothing to copy.
    SourceMissing,
    /// Failed after the target had changed: `Entry::interrupted`.
    Interrupted(String),
}

/// What a copy wrote.
#[derive(Debug, PartialEq)]
pub struct Copied {
    /// The copy's new stamp.
    pub stamp: Stamp,
    /// The referenced files now beside it (`Entry::assets`).
    pub assets: Vec<AssetCopy>,
    /// References not copied because a different file Bindle didn't write
    /// for this document is already at that path.
    pub clashes: Vec<PathBuf>,
    /// The backup folder's mount point, for `Job::mount` when none was
    /// recorded yet (settings from before it was).
    pub mount: Option<PathBuf>,
}

/// Copy `src` (a file or a bundle directory) to `folder/name`. Unless
/// `force`, refuses to replace a copy whose stamp is not `last` and whose
/// content differs (a copy with the same content is ours whatever its
/// stamp says: written by another instance, a quit, or re-flagged). A plain
/// markdown file brings along the local files it references inside its
/// own folder (images, attachments), at the same relative paths, so its
/// links keep working in the backup folder.
pub fn copy(job: &Job) -> io::Result<Outcome> {
    let _one_at_a_time = COPY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !job.folder.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "the backup folder is not available"));
    }
    let mount = mount_point(&job.folder);
    if let (Some(was), Some(now)) = (&job.mount, &mount)
        && was != now
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the drive is not mounted at {} (choose the folder again if it moved)", was.display()),
        ));
    }
    let meta = match fs::metadata(&job.src) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Outcome::SourceMissing),
        other => other?,
    };
    let target = job.folder.join(&job.name);
    let before = target_stamp(&target);
    if !job.force
        && !job.interrupted
        && let Some(now) = before
        && Some(now) != job.last
        && !same_tree(&job.src, &target)
    {
        return Ok(Outcome::Changed);
    }
    let written = if meta.is_dir() { copy_dir(&job.src, &target, job.force) } else { copy_file(&job.src, &target) };
    if let Err(err) = written {
        // A temp file that never got renamed leaves the target as it was;
        // an in-place write or a bundle half copied file by file does not.
        return if target_stamp(&target) == before { Err(err) } else { Ok(Outcome::Interrupted(err.to_string())) };
    }
    let (assets, clashes) = if !meta.is_dir() && is_plain_markdown(&job.src) { copy_assets(job) } else { Default::default() };
    let stamp = target_stamp(&target).ok_or_else(|| io::Error::other("the copy could not be read back"))?;
    Ok(Outcome::Copied(Copied { stamp, assets, clashes, mount }))
}

/// The mount point holding `path`: the longest mount point in
/// `/proc/self/mountinfo` that the canonical path lies under. Mount point
/// paths are stable across reboots, unlike device numbers (anonymous on
/// btrfs subvolumes and FUSE, handed out in mount order).
pub fn mount_point(path: &Path) -> Option<PathBuf> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    containing_mount(&info, &fs::canonicalize(path).ok()?)
}

fn containing_mount(mountinfo: &str, path: &Path) -> Option<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape_mount)
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.as_os_str().len())
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape_mount(field: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = |c: u8| (b'0'..=b'7').contains(&c);
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|&c| octal(c)) {
            let v = (b[i + 1] - b'0') as u16 * 64 + (b[i + 2] - b'0') as u16 * 8 + (b[i + 3] - b'0') as u16;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    PathBuf::from(OsString::from_vec(out))
}

/// Same content: two files with the same bytes, or two directories holding
/// the same names with the same content all the way down.
fn same_tree(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else { return false };
    match (ma.is_dir(), mb.is_dir()) {
        (false, false) => same_bytes(a, b),
        (true, true) => {
            let names = |dir: &Path| -> Option<std::collections::BTreeSet<OsString>> {
                fs::read_dir(dir).ok()?.map(|e| e.ok().map(|e| e.file_name())).collect()
            };
            match (names(a), names(b)) {
                (Some(na), Some(nb)) if na == nb => na.iter().all(|n| same_tree(&a.join(n), &b.join(n))),
                _ => false,
            }
        }
        _ => false,
    }
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(fa), Ok(fb)) = (fs::File::open(a), fs::File::open(b)) else { return false };
    let (mut ra, mut rb) = (io::BufReader::new(fa), io::BufReader::new(fb));
    loop {
        let (Ok(ba), Ok(bb)) = (ra.fill_buf(), rb.fill_buf()) else { return false };
        let n = ba.len().min(bb.len());
        if n == 0 {
            return ba.is_empty() && bb.is_empty();
        }
        if ba[..n] != bb[..n] {
            return false;
        }
        ra.consume(n);
        rb.consume(n);
    }
}

/// The copy gets the document's permissions, so a private file doesn't turn
/// world-readable in the backup folder. Best effort: cloud and FAT folders
/// may not keep modes at all.
fn keep_mode(src: &Path, target: &Path) {
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(target, fs::Permissions::from_mode(meta.permissions().mode() & 0o777));
    }
}

/// A markdown text file, whose references live beside it — not a pack or
/// an .odt, which carry their pictures inside.
fn is_plain_markdown(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase());
    matches!(ext.as_deref(), None | Some("md" | "markdown" | "mdown" | "mkd"))
}

/// The stamp of a copy: the file's, or for a bundle directory the newest
/// mtime and total size of everything in it (a directory's own mtime says
/// nothing about a rewritten file inside).
pub fn target_stamp(target: &Path) -> Option<Stamp> {
    let meta = fs::metadata(target).ok()?;
    if !meta.is_dir() {
        return Some(stamp_of(meta.modified().ok()?, meta.len()));
    }
    let mut newest = UNIX_EPOCH;
    let mut size = 0;
    let mut stack = vec![target.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                newest = newest.max(meta.modified().unwrap_or(UNIX_EPOCH));
                size += meta.len();
            }
        }
    }
    Some(stamp_of(newest, size))
}

/// Replace `target` with `src`'s bytes without leaving a half-written copy
/// (temp + rename, in place on backends that can't rename).
fn copy_file(src: &Path, target: &Path) -> io::Result<()> {
    crate::doc::replace_or_in_place(target, |out| {
        let mut input = fs::File::open(src)?;
        io::copy(&mut input, out).map(|_| ())
    })?;
    keep_mode(src, target);
    Ok(())
}

/// A bundle, file by file into `target`, then whatever the bundle no longer
/// has is removed from the copy. File by file rather than one directory
/// rename, which some network backends refuse. Unchanged files are skipped
/// by size and age, except when `force` replaces a copy edited elsewhere —
/// an edit that kept the size would pass that test, so then only identical
/// bytes are skipped.
fn copy_dir(src: &Path, target: &Path, force: bool) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let mut keep = std::collections::HashSet::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        keep.insert(entry.file_name());
        let unchanged = if force { same_bytes(&from, &to) } else { same_file_content(&from, &to) };
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to, force)?;
        } else if !unchanged {
            copy_file(&from, &to)?;
        }
    }
    for entry in fs::read_dir(target)?.flatten() {
        if !keep.contains(&entry.file_name()) {
            let path = entry.path();
            let _ = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        }
    }
    Ok(())
}

/// Same size and the copy no older: copying again would change nothing.
fn same_file_content(src: &Path, copy: &Path) -> bool {
    match (fs::metadata(src), fs::metadata(copy)) {
        (Ok(a), Ok(b)) => {
            a.len() == b.len()
                && matches!((a.modified(), b.modified()), (Ok(ma), Ok(mb)) if mb >= ma)
        }
        _ => false,
    }
}

/// The local files a markdown document references inside its own folder,
/// copied to the same relative paths under the backup folder. Best effort:
/// a missing image doesn't fail the document's backup.
///
/// The backup folder is shared by every flagged document, so a path may
/// already hold another document's file of the same name, or the user's
/// own. Only a file this document's last copy wrote (`Job::assets`, stamp
/// unchanged) is replaced; one with the same bytes is adopted; anything
/// else is left alone and reported as a clash. References that resolve
/// outside the document's folder (through a symlink) are never copied.
fn copy_assets(job: &Job) -> (Vec<AssetCopy>, Vec<PathBuf>) {
    let (mut copied, mut clashes) = (Vec::new(), Vec::new());
    let Ok(text) = fs::read_to_string(&job.src) else { return (copied, clashes) };
    let Some(base) = job.src.parent().and_then(|b| fs::canonicalize(b).ok()) else {
        return (copied, clashes);
    };
    for rel in asset_paths(&text) {
        if lands_on_reserved(&rel, &job.reserved) {
            continue;
        }
        let Ok(from) = fs::canonicalize(base.join(&rel)) else { continue };
        if !from.starts_with(&base) || !from.is_file() || from.strip_prefix(&base).is_ok_and(has_hidden) {
            continue;
        }
        let to = job.folder.join(&rel);
        let ours = job.assets.iter().find(|a| a.path == rel).map(|a| a.stamp);
        let write = match fs::metadata(&to) {
            Err(_) => true,
            Ok(meta) if meta.is_dir() => {
                clashes.push(rel);
                continue;
            }
            Ok(meta) => {
                let now = meta.modified().ok().map(|m| stamp_of(m, meta.len()));
                if now.is_some() && now == ours {
                    !same_file_content(&from, &to)
                } else if same_bytes(&from, &to) {
                    false
                } else {
                    clashes.push(rel);
                    continue;
                }
            }
        };
        if write {
            if let Some(dir) = to.parent() {
                let _ = fs::create_dir_all(dir);
            }
            // A failed write still records what is there now as ours: if it
            // was half written in place, the next copy replaces it rather
            // than calling it a clash.
            let _ = copy_file(&from, &to);
        }
        if let Ok(meta) = fs::metadata(&to)
            && let Ok(mtime) = meta.modified()
        {
            copied.push(AssetCopy { path: rel, stamp: stamp_of(mtime, meta.len()) });
        }
    }
    (copied, clashes)
}

/// A reference whose first component is a flagged document's copy (another
/// document linked to by name, or a file inside a bundle's copy).
fn lands_on_reserved(rel: &Path, reserved: &[PathBuf]) -> bool {
    let Some(first) = rel.components().find_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
        _ => None,
    }) else {
        return false;
    };
    reserved.iter().any(|r| r.as_os_str().to_string_lossy().to_lowercase() == first)
}

/// A hidden file or folder anywhere on the path (`.ssh/…`, `.config/…`).
fn has_hidden(rel: &Path) -> bool {
    rel.components().any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with('.')))
}

/// Relative paths of the local references worth copying beside a document:
/// inside its folder — no absolute paths, no `..` — so a copy can't land
/// outside the backup folder; nothing hidden, since a document in the home
/// folder could otherwise send `.ssh/id_ed25519` to a cloud folder; and no
/// links to other markdown documents, which are documents, not attachments.
pub fn asset_paths(markdown: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dest in crate::bundle::local_asset_dests(markdown) {
        let rel = PathBuf::from(crate::bundle::decode_dest(&dest));
        let inside = rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        let document = is_plain_markdown(&rel) || {
            let ext = rel.extension().map(|e| e.to_string_lossy().to_lowercase());
            matches!(ext.as_deref(), Some("textbundle" | "textpack"))
        };
        if inside && !document && !has_hidden(&rel) && rel.components().next().is_some() && !out.contains(&rel) {
            out.push(rel);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_name_is_kept() {
        assert_eq!(name_for(Path::new("/home/u/Work/notes.md"), &[]), PathBuf::from("notes.md"));
    }

    #[test]
    fn a_clash_takes_the_parent_folder_then_two_then_a_number() {
        let taken = [Path::new("notes.md")];
        assert_eq!(name_for(Path::new("/h/Work/notes.md"), &taken), PathBuf::from("notes (Work).md"));
        let taken = [Path::new("Notes.md"), Path::new("notes (x).md")];
        assert_eq!(name_for(Path::new("/a/x/notes.md"), &taken), PathBuf::from("notes (a-x).md"));
        let taken = [Path::new("notes.md"), Path::new("notes (x).md"), Path::new("notes (a-x).md")];
        assert_eq!(name_for(Path::new("/a/x/notes.md"), &taken), PathBuf::from("notes (2).md"));
    }

    #[test]
    fn a_bundle_keeps_its_extension() {
        let taken = [Path::new("trip.textbundle")];
        assert_eq!(
            name_for(Path::new("/h/Travel/trip.textbundle"), &taken),
            PathBuf::from("trip (Travel).textbundle")
        );
    }

    #[test]
    fn only_references_inside_the_document_folder_are_assets() {
        let md = "![a](assets/a.png) ![b](../b.png) ![c](/etc/c.png) [d](docs/d%20e.pdf) ![w](https://x/y.png) \
                  [k](.ssh/id_ed25519) [h](x/.git/config) [n](other.md) [t](trip.textbundle/text.md)";
        assert_eq!(asset_paths(md), vec![PathBuf::from("assets/a.png"), PathBuf::from("docs/d e.pdf")]);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bindle-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn job(src: &Path, folder: &Path, name: &str, last: Option<Stamp>, force: bool) -> Job {
        Job {
            src: src.to_path_buf(),
            folder: folder.to_path_buf(),
            name: name.into(),
            last,
            assets: Vec::new(),
            reserved: vec![name.into()],
            force,
            interrupted: false,
            mount: None,
        }
    }

    fn copied(outcome: io::Result<Outcome>) -> Copied {
        match outcome.unwrap() {
            Outcome::Copied(c) => c,
            other => panic!("not copied: {other:?}"),
        }
    }

    #[test]
    fn copies_a_document_with_its_images_and_refuses_a_copy_changed_elsewhere() {
        let dir = scratch("copy");
        let docs = dir.join("docs");
        let backup = dir.join("backup");
        fs::create_dir_all(docs.join("assets")).unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::write(docs.join("assets/a.png"), b"png").unwrap();
        let doc = docs.join("n.md");
        fs::write(&doc, "![a](assets/a.png)").unwrap();
        fs::set_permissions(&doc, fs::Permissions::from_mode(0o600)).unwrap();

        let first = copied(copy(&job(&doc, &backup, "n.md", None, false)));
        assert_eq!(fs::read(backup.join("assets/a.png")).unwrap(), b"png");
        assert_eq!(first.assets.len(), 1);
        assert_eq!(fs::metadata(backup.join("n.md")).unwrap().permissions().mode() & 0o777, 0o600);
        fs::write(&doc, "![a](assets/a.png) more").unwrap();
        let stamp = copied(copy(&job(&doc, &backup, "n.md", Some(first.stamp), false))).stamp;
        assert_eq!(fs::read_to_string(backup.join("n.md")).unwrap(), "![a](assets/a.png) more");

        // Edited in the backup folder: not overwritten unless forced.
        fs::write(backup.join("n.md"), "theirs, longer").unwrap();
        assert_eq!(copy(&job(&doc, &backup, "n.md", Some(stamp), false)).unwrap(), Outcome::Changed);
        assert_eq!(fs::read_to_string(backup.join("n.md")).unwrap(), "theirs, longer");
        copied(copy(&job(&doc, &backup, "n.md", Some(stamp), true)));
        // A file of that name Bindle never wrote is not ours either.
        fs::write(backup.join("m.md"), "other").unwrap();
        assert_eq!(copy(&job(&doc, &backup, "m.md", None, false)).unwrap(), Outcome::Changed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_with_the_same_content_is_ours_whatever_its_stamp() {
        // Re-flagged, written by another instance or by the last try at
        // quit: the stamp is stale or missing, the bytes are the same.
        let dir = scratch("same");
        let backup = dir.join("b");
        fs::create_dir_all(&backup).unwrap();
        let doc = dir.join("n.md");
        fs::write(&doc, "text").unwrap();
        fs::write(backup.join("n.md"), "text").unwrap();
        copied(copy(&job(&doc, &backup, "n.md", None, false)));
        copied(copy(&job(&doc, &backup, "n.md", Some((1, 2, 3)), false)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reference_never_replaces_a_file_it_did_not_write() {
        let dir = scratch("clash");
        let (a, b, backup) = (dir.join("a"), dir.join("b"), dir.join("backup"));
        for d in [&a, &b] {
            fs::create_dir_all(d.join("img")).unwrap();
        }
        fs::create_dir_all(&backup).unwrap();
        fs::write(a.join("img/d.png"), b"from a").unwrap();
        fs::write(b.join("img/d.png"), b"from b!").unwrap();
        fs::write(a.join("n.md"), "![d](img/d.png)").unwrap();
        fs::write(b.join("n.md"), "![d](img/d.png)").unwrap();

        let ca = copied(copy(&job(&a.join("n.md"), &backup, "n.md", None, false)));
        let cb = copied(copy(&job(&b.join("n.md"), &backup, "n (b).md", None, false)));
        assert_eq!(fs::read(backup.join("img/d.png")).unwrap(), b"from a");
        assert_eq!(cb.clashes, vec![PathBuf::from("img/d.png")]);
        assert!(cb.assets.is_empty());

        // A's own file, changed at the source, is replaced.
        fs::write(a.join("img/d.png"), b"from a, v2").unwrap();
        let mut again = job(&a.join("n.md"), &backup, "n.md", Some(ca.stamp), false);
        again.assets = ca.assets;
        let ca = copied(copy(&again));
        assert!(ca.clashes.is_empty());
        assert_eq!(fs::read(backup.join("img/d.png")).unwrap(), b"from a, v2");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn references_through_a_symlink_out_of_the_folder_or_onto_a_copy_stay_behind() {
        let dir = scratch("escape");
        let (docs, backup) = (dir.join("docs"), dir.join("backup"));
        fs::create_dir_all(&docs).unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::write(dir.join("secret"), "key").unwrap();
        std::os::unix::fs::symlink(dir.join("secret"), docs.join("x.png")).unwrap();
        fs::create_dir_all(docs.join("trip.textbundle")).unwrap();
        fs::write(docs.join("trip.textbundle/info.json"), "{}").unwrap();
        fs::write(docs.join("n.md"), "![x](x.png) [i](trip.textbundle/info.json)").unwrap();
        let mut j = job(&docs.join("n.md"), &backup, "n.md", None, false);
        j.reserved.push("trip.textbundle".into());
        let c = copied(copy(&j));
        assert!(c.assets.is_empty() && c.clashes.is_empty());
        assert!(!backup.join("x.png").exists());
        assert!(!backup.join("trip.textbundle").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_copy_follows_removed_files() {
        let dir = scratch("bundle");
        let bundle = dir.join("t.textbundle");
        fs::create_dir_all(bundle.join("assets")).unwrap();
        fs::write(bundle.join("text.md"), "x").unwrap();
        fs::write(bundle.join("assets/old.png"), "o").unwrap();
        let backup = dir.join("b");
        fs::create_dir_all(&backup).unwrap();
        let stamp = copied(copy(&job(&bundle, &backup, "t.textbundle", None, false))).stamp;
        fs::remove_file(bundle.join("assets/old.png")).unwrap();
        copied(copy(&job(&bundle, &backup, "t.textbundle", Some(stamp), false)));
        assert!(backup.join("t.textbundle/text.md").is_file());
        assert!(!backup.join("t.textbundle/assets/old.png").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unavailable_folder_is_an_error_and_a_gone_document_is_not() {
        let dir = scratch("gone");
        let doc = dir.join("n.md");
        fs::write(&doc, "x").unwrap();
        assert!(copy(&job(&doc, &dir.join("missing"), "n.md", None, false)).is_err());
        let b = dir.join("b");
        fs::create_dir_all(&b).unwrap();
        assert_eq!(copy(&job(&dir.join("nope.md"), &b, "nope.md", None, false)).unwrap(), Outcome::SourceMissing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mount_point_is_the_longest_one_holding_the_path() {
        let info = "22 1 0:21 / / rw - btrfs /dev/vda3 rw\n\
                    40 22 8:17 / /mnt/back\\040up rw - ext4 /dev/sdb1 rw\n\
                    41 22 0:50 / /mnt/back rw - tmpfs tmpfs rw\n";
        assert_eq!(containing_mount(info, Path::new("/mnt/back up/notes")), Some(PathBuf::from("/mnt/back up")));
        assert_eq!(containing_mount(info, Path::new("/mnt/backup/notes")), Some(PathBuf::from("/")));
        assert_eq!(unescape_mount("a\\134b\\011"), PathBuf::from("a\\b\t"));
    }

    #[test]
    fn a_folder_off_its_drive_is_unavailable() {
        // The drive unplugged under a fixed mount point: the folder is the
        // empty directory on the disk beneath, on another mount point.
        let dir = scratch("unmounted");
        let doc = dir.join("n.md");
        fs::write(&doc, "x").unwrap();
        let backup = dir.join("b");
        fs::create_dir_all(&backup).unwrap();
        let mut j = job(&doc, &backup, "n.md", None, false);
        j.mount = Some(PathBuf::from("/no/such/mount"));
        let err = copy(&j).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_dir(&backup).unwrap().count(), 0, "nothing written");
        // Recorded as found, it copies, and reports the mount point.
        j.mount = mount_point(&backup);
        assert_eq!(copied(copy(&j)).mount, j.mount);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_cut_off_halfway_is_replaced_without_asking() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("interrupted");
        let bundle = dir.join("t.textbundle");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("text.md"), "x").unwrap();
        let locked = bundle.join("locked.png");
        fs::write(&locked, "secret").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let backup = dir.join("b");
        fs::create_dir_all(&backup).unwrap();
        let outcome = copy(&job(&bundle, &backup, "t.textbundle", None, false)).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        if fs::File::open(&locked).is_ok() && matches!(outcome, Outcome::Copied(_)) {
            return; // running as root: nothing could be refused
        }
        assert!(matches!(outcome, Outcome::Interrupted(_)), "{outcome:?}");
        // No stamp was recorded and the half copy differs from the bundle:
        // without the flag that reads as changed elsewhere.
        assert_eq!(copy(&job(&bundle, &backup, "t.textbundle", None, false)).unwrap(), Outcome::Changed);
        let mut again = job(&bundle, &backup, "t.textbundle", None, false);
        again.interrupted = true;
        copied(copy(&again));
        assert_eq!(fs::read(backup.join("t.textbundle/locked.png")).unwrap(), b"secret");
        let _ = fs::remove_dir_all(&dir);
    }
}
