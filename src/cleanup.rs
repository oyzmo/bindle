//! "Clean Up Assets…": files in a document's `assets/` that nothing refers
//! to any more. GTK-free and unit-tested; `ui.rs` shows the list and moves
//! what the user ticks to the Trash.
//!
//! Deliberately conservative. A file counts as used when its name appears
//! *anywhere* in the text (raw, percent-encoded, or as a parsed link), not
//! only in a link the scanner understands, so raw HTML, code blocks and odd
//! escapes keep a file rather than lose it.
//!
//! - **Bundle / pack**: `assets/` belongs to the one document, so every file
//!   the text doesn't mention is listed.
//! - **Plain file**: `assets/` is shared by every document in the folder and
//!   whatever else lives there, so only images *pasted into this document*
//!   (named `<doc-stem>-<stamp>.png` by `bundle::pasted_image_name`) and
//!   Bindle's generic `pasted-<stamp>.png` are considered, and only when no
//!   text file in the folder mentions them.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::bundle;

/// An unused file: `rel` is its path relative to the document's container
/// (`assets/x.png`), `size` in bytes where known.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub rel: String,
    pub size: Option<u64>,
}

/// How many files a plain document's folder may hold before the scan gives
/// up rather than list something it could not check against everything.
pub const SCAN_MAX_FILES: usize = 2000;
/// Folder depth searched below a plain document's folder.
const SCAN_DEPTH: usize = 3;
/// Text files larger than this are not read (logs, dumps).
const SCAN_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Extensions of files that may refer to an asset by name.
const TEXT_EXTS: &[&str] = &[
    "md", "markdown", "mdown", "mkd", "txt", "text", "html", "htm", "xhtml", "css", "js", "json",
    "yaml", "yml", "toml", "xml", "svg", "org", "rst", "adoc", "tex", "csv",
];

/// The texts searched, with the file names their links point at.
pub struct Mentions {
    texts: Vec<String>,
    linked: HashSet<String>,
}

impl Mentions {
    pub fn new(texts: Vec<String>) -> Self {
        let linked = texts
            .iter()
            .flat_map(|t| bundle::local_asset_dests(t))
            .filter_map(|d| {
                let d = bundle::decode_dest(&d);
                Path::new(&d).file_name().map(|n| n.to_string_lossy().into_owned())
            })
            .collect();
        Mentions { texts, linked }
    }

    /// Whether the file name `name` is referred to anywhere.
    pub fn mention(&self, name: &str) -> bool {
        if self.linked.contains(name) {
            return true;
        }
        let encoded = percent_encode(name);
        self.texts.iter().any(|t| t.contains(name) || t.contains(&encoded))
    }
}

fn percent_encode(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// Whether `name` is an image pasted into the document at `doc`:
/// `<stem>-YYYYMMDD-HHMMSS.png`, optionally numbered (`-2`). Documents
/// whose name leaves no usable stem share the generic `pasted-` prefix with
/// every unsaved paste (and older Bindle versions), so they own nothing.
pub fn pasted_by(doc: &Path, name: &str) -> bool {
    let probe = bundle::pasted_image_name(Some(doc), "0");
    let prefix = &probe[..probe.len() - "-0.png".len()];
    if prefix == "pasted" {
        return false;
    }
    let Some(rest) = name
        .strip_prefix(prefix)
        .and_then(|r| r.strip_prefix('-'))
        .and_then(|r| r.strip_suffix(".png"))
    else {
        return false;
    };
    is_stamp(rest)
}

/// Whether `name` is an image Bindle pasted under the generic name —
/// `pasted-<stamp>.png`, from before pasted images were named after their
/// document, or a paste into a document whose name left no usable stem.
/// Only Bindle makes these, but they can belong to any document in the
/// folder, so `plain_unused` lists them only when no file there uses them.
pub fn generic_paste(name: &str) -> bool {
    name.strip_prefix("pasted-").and_then(|r| r.strip_suffix(".png")).is_some_and(is_stamp)
}

/// `YYYYMMDD-HHMMSS`, optionally numbered (`-2`).
fn is_stamp(rest: &str) -> bool {
    let b = rest.as_bytes();
    if b.len() < 15 {
        return false;
    }
    let stamp = b[..15].iter().enumerate().all(|(i, c)| if i == 8 { *c == b'-' } else { c.is_ascii_digit() });
    let tail = &b[15..];
    stamp && (tail.is_empty() || (tail.len() > 1 && tail[0] == b'-' && tail[1..].iter().all(u8::is_ascii_digit)))
}

/// Files below `dir/assets` (recursively) that `text` doesn't mention.
pub fn bundle_unused(dir: &Path, text: &str) -> Vec<Candidate> {
    let mentions = Mentions::new(vec![text.to_string()]);
    let mut files = Vec::new();
    list_files(&dir.join("assets"), "assets", &mut files);
    files
        .into_iter()
        .filter(|(rel, _)| !mentions.mention(file_name(rel)))
        .map(|(rel, size)| Candidate { rel, size: Some(size) })
        .collect()
}

/// Entries of a pack's `assets/` (names relative to `prefix`) that `text`
/// doesn't mention.
pub fn pack_unused(entries: &[String], prefix: &str, text: &str) -> Vec<Candidate> {
    let mentions = Mentions::new(vec![text.to_string()]);
    entries
        .iter()
        .filter_map(|e| e.strip_prefix(prefix))
        .filter(|rel| rel.starts_with("assets/") && !rel.ends_with('/'))
        .filter(|rel| !mentions.mention(file_name(rel)))
        .map(|rel| Candidate { rel: rel.to_string(), size: None })
        .collect()
}

/// Why a plain document's folder could not be checked.
#[derive(Debug, PartialEq)]
pub enum ScanError {
    /// More than `SCAN_MAX_FILES` files to search.
    TooMany,
}

/// Images pasted into the plain document `doc` (whose current text is
/// `text`) that neither it nor any text file in its folder mentions.
pub fn plain_unused(doc: &Path, text: &str) -> Result<Vec<Candidate>, ScanError> {
    let Some(folder) = doc.parent() else { return Ok(Vec::new()) };
    let mut own = Vec::new();
    if let Ok(dir) = fs::read_dir(folder.join("assets")) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() && (pasted_by(doc, &name) || generic_paste(&name)) {
                own.push((format!("assets/{name}"), meta.len()));
            }
        }
    }
    if own.is_empty() {
        return Ok(Vec::new());
    }
    // The document's own file on disk is skipped in favour of `text` (the
    // app only scans a saved document, so the two agree).
    let mut texts = vec![text.to_string()];
    let mut seen = 0;
    gather_texts(folder, doc, SCAN_DEPTH, &mut seen, &mut texts)?;
    let mentions = Mentions::new(texts);
    own.sort();
    Ok(own
        .into_iter()
        .filter(|(rel, _)| !mentions.mention(file_name(rel)))
        .map(|(rel, size)| Candidate { rel, size: Some(size) })
        .collect())
}

fn gather_texts(dir: &Path, skip: &Path, depth: usize, seen: &mut usize, out: &mut Vec<String>) -> Result<(), ScanError> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > SCAN_MAX_FILES {
            return Err(ScanError::TooMany);
        }
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        // Not followed through symlinks: a link to `~` would scan the home.
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if depth > 0 {
                gather_texts(&path, skip, depth - 1, seen, out)?;
            }
            continue;
        }
        if path == skip {
            continue;
        }
        let text_like = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| TEXT_EXTS.contains(&e.to_ascii_lowercase().as_str()));
        if !text_like {
            continue;
        }
        if let Some(bytes) = bundle::read_file_capped(&path, SCAN_MAX_BYTES) {
            out.push(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    Ok(())
}

fn list_files(dir: &Path, rel: &str, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = format!("{rel}/{name}");
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            list_files(&entry.path(), &child, out);
        } else if let Ok(meta) = entry.metadata() {
            out.push((child, meta.len()));
        } else {
            out.push((child, 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bindle-cleanup-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("assets")).unwrap();
        dir
    }

    #[test]
    fn a_name_anywhere_counts_as_used() {
        let m = Mentions::new(vec![
            "![x](assets/a%20b.png) <img src=\"assets/raw.png\">\n```\nassets/code.png\n```\n![](assets/c\\_d.png)"
                .to_string(),
        ]);
        assert!(m.mention("a b.png"));
        assert!(m.mention("raw.png"));
        assert!(m.mention("code.png"));
        assert!(m.mention("c_d.png"));
        assert!(!m.mention("gone.png"));
    }

    #[test]
    fn only_this_documents_pastes_belong_to_it() {
        let doc = Path::new("/d/temp.md");
        assert!(pasted_by(doc, "temp-20261125-082145.png"));
        assert!(pasted_by(doc, "temp-20261125-082145-2.png"));
        assert!(!pasted_by(doc, "temp-20261125-082145-.png"));
        assert!(!pasted_by(doc, "temp-2026.png"));
        assert!(!pasted_by(doc, "temp-notes-20261125-082145.png"));
        assert!(!pasted_by(doc, "other-20261125-082145.png"));
        assert!(!pasted_by(doc, "temp-20261125-082145.jpg"));
        // Another document whose stem starts with ours.
        assert!(!pasted_by(doc, "temp-20261125-20261125-082145.png"));
        // A stem that leaves nothing owns nothing.
        assert!(!pasted_by(Path::new("/d/???.md"), "pasted-20261125-082145.png"));
        assert!(generic_paste("pasted-20261125-082145.png"));
        assert!(generic_paste("pasted-20261125-082145-3.png"));
        assert!(!generic_paste("pasted-holiday.png"));
    }

    #[test]
    fn plain_lists_only_own_pastes_nothing_else_mentions() {
        let dir = scratch("plain");
        let doc = dir.join("temp.md");
        fs::write(&doc, "on disk: temp-20261125-000001.png").unwrap();
        for name in [
            "temp-20261125-000001.png", // only the saved file mentions it
            "temp-20261125-000002.png", // the current text mentions it
            "temp-20261125-000003.png", // a sibling document mentions it
            "temp-20261125-000004.png", // a document in a subfolder does
            "temp-20261125-000005.png", // nobody
            "pasted-20261125-000006.png", // generic paste, nobody
            "pasted-20261125-000007.png", // generic paste, a sibling uses it
            "logo.png",                 // not ours
        ] {
            fs::write(dir.join("assets").join(name), b"x").unwrap();
        }
        fs::write(dir.join("other.md"), "![](assets/temp-20261125-000003.png) ![](assets/pasted-20261125-000007.png)").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/page.html"), "<img src=\"../assets/temp-20261125-000004.png\">").unwrap();
        let found = plain_unused(&doc, "![](assets/temp-20261125-000002.png)").unwrap();
        let rels: Vec<_> = found.iter().map(|c| c.rel.as_str()).collect();
        assert_eq!(
            rels,
            ["assets/pasted-20261125-000006.png", "assets/temp-20261125-000001.png", "assets/temp-20261125-000005.png"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundle_and_pack_list_every_unmentioned_asset() {
        let dir = scratch("bundle");
        fs::create_dir_all(dir.join("assets/sub")).unwrap();
        fs::write(dir.join("assets/used.png"), b"1").unwrap();
        fs::write(dir.join("assets/old.pdf"), b"22").unwrap();
        fs::write(dir.join("assets/sub/deep.png"), b"333").unwrap();
        let found = bundle_unused(&dir, "![](assets/used.png)");
        assert_eq!(
            found,
            [
                Candidate { rel: "assets/old.pdf".into(), size: Some(2) },
                Candidate { rel: "assets/sub/deep.png".into(), size: Some(3) },
            ]
        );
        let entries: Vec<String> =
            ["P/text.md", "P/info.json", "P/assets/", "P/assets/used.png", "P/assets/old.pdf"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let found = pack_unused(&entries, "P/", "![](assets/used.png)");
        assert_eq!(found, [Candidate { rel: "assets/old.pdf".into(), size: None }]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_too_large_to_check_lists_nothing() {
        let dir = scratch("large");
        let doc = dir.join("temp.md");
        fs::write(dir.join("assets/temp-20261125-000001.png"), b"x").unwrap();
        for i in 0..SCAN_MAX_FILES {
            fs::write(dir.join(format!("f{i}.bin")), b"").unwrap();
        }
        assert_eq!(plain_unused(&doc, ""), Err(ScanError::TooMany));
        let _ = fs::remove_dir_all(&dir);
    }
}
