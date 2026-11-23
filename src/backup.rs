//! Backup copies: documents the user flags in the Files panel are copied to
//! one backup folder after every save — the latest copy only, one-way.
//! GTK-free and unit-tested; `ui.rs` decides when, runs `copy` on a worker
//! thread (the folder may sit on a network share that hangs) and asks the
//! user when `copy` finds the copy was changed by someone else.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
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
}

/// `Entry::stamp`.
pub type Stamp = (u64, u32, u64);

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
    /// Written; the copy's new stamp.
    Copied(Stamp),
    /// Not written: something is already at the target that Bindle did not
    /// write last (edited elsewhere, or an unrelated file of the same name).
    /// Run again with `force` to replace it.
    Changed,
}

/// Copy `src` (a file or a bundle directory) to `folder/name`. Unless
/// `force`, refuses to replace a copy whose stamp is not `last`. A plain
/// markdown file brings along the local files it references inside its
/// own folder (images, attachments), at the same relative paths, so its
/// links keep working in the backup folder.
pub fn copy(src: &Path, folder: &Path, name: &Path, last: Option<Stamp>, force: bool) -> io::Result<Outcome> {
    if !folder.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "the backup folder is not available"));
    }
    let target = folder.join(name);
    if !force && let Some(now) = target_stamp(&target)
        && Some(now) != last
    {
        return Ok(Outcome::Changed);
    }
    let meta = fs::metadata(src)?;
    if meta.is_dir() {
        copy_dir(src, &target)?;
    } else {
        copy_file(src, &target)?;
        if is_plain_markdown(src) {
            copy_assets(src, folder);
        }
    }
    target_stamp(&target)
        .map(Outcome::Copied)
        .ok_or_else(|| io::Error::other("the copy could not be read back"))
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
    })
}

/// A bundle, file by file into `target`, then whatever the bundle no longer
/// has is removed from the copy. File by file rather than one directory
/// rename, which some network backends refuse.
fn copy_dir(src: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let mut keep = std::collections::HashSet::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        keep.insert(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else if !same_file_content(&from, &to) {
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
/// copied to the same relative paths under `folder`. Best effort: a missing
/// image doesn't fail the document's backup.
fn copy_assets(doc: &Path, folder: &Path) {
    let Ok(text) = fs::read_to_string(doc) else { return };
    let Some(base) = doc.parent() else { return };
    for rel in asset_paths(&text) {
        let from = base.join(&rel);
        if !from.is_file() {
            continue;
        }
        let to = folder.join(&rel);
        if same_file_content(&from, &to) {
            continue;
        }
        if let Some(dir) = to.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = copy_file(&from, &to);
    }
}

/// Relative paths of the local references that stay inside the document's
/// folder — no absolute paths, no `..` — so a copy can't land outside the
/// backup folder.
pub fn asset_paths(markdown: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dest in crate::bundle::local_asset_dests(markdown) {
        let rel = PathBuf::from(crate::bundle::decode_dest(&dest));
        let inside = rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if inside && rel.components().next().is_some() && !out.contains(&rel) {
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
        let md = "![a](assets/a.png) ![b](../b.png) ![c](/etc/c.png) [d](docs/d%20e.pdf) ![w](https://x/y.png)";
        assert_eq!(asset_paths(md), vec![PathBuf::from("assets/a.png"), PathBuf::from("docs/d e.pdf")]);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bindle-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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

        let Outcome::Copied(stamp) = copy(&doc, &backup, Path::new("n.md"), None, false).unwrap() else {
            panic!("not copied")
        };
        assert_eq!(fs::read(backup.join("assets/a.png")).unwrap(), b"png");
        fs::write(&doc, "![a](assets/a.png) more").unwrap();
        assert!(matches!(copy(&doc, &backup, Path::new("n.md"), Some(stamp), false).unwrap(), Outcome::Copied(_)));
        assert_eq!(fs::read_to_string(backup.join("n.md")).unwrap(), "![a](assets/a.png) more");

        // Edited in the backup folder: not overwritten unless forced.
        fs::write(backup.join("n.md"), "theirs, longer").unwrap();
        assert_eq!(copy(&doc, &backup, Path::new("n.md"), Some(stamp), false).unwrap(), Outcome::Changed);
        assert_eq!(fs::read_to_string(backup.join("n.md")).unwrap(), "theirs, longer");
        assert!(matches!(copy(&doc, &backup, Path::new("n.md"), Some(stamp), true).unwrap(), Outcome::Copied(_)));
        // A file of that name Bindle never wrote is not ours either.
        fs::write(backup.join("m.md"), "other").unwrap();
        assert_eq!(copy(&doc, &backup, Path::new("m.md"), None, false).unwrap(), Outcome::Changed);
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
        let Outcome::Copied(stamp) = copy(&bundle, &backup, Path::new("t.textbundle"), None, false).unwrap() else {
            panic!()
        };
        fs::remove_file(bundle.join("assets/old.png")).unwrap();
        copy(&bundle, &backup, Path::new("t.textbundle"), Some(stamp), false).unwrap();
        assert!(backup.join("t.textbundle/text.md").is_file());
        assert!(!backup.join("t.textbundle/assets/old.png").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unavailable_folder_is_an_error() {
        let dir = scratch("gone");
        let doc = dir.join("n.md");
        fs::write(&doc, "x").unwrap();
        assert!(copy(&doc, &dir.join("missing"), Path::new("n.md"), None, false).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
