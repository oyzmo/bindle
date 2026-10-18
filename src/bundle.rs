//! TextBundle (folder) and TextPack (zip) support: open, save, export.
//! Spec: <https://textbundle.org> — `info.json` + `text.md` + `assets/`.
//! Packs are never extracted to disk: the text is read once and assets are
//! read lazily, entry by entry, when the preview needs them.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use pulldown_cmark::{Event, Options, Parser, Tag};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

/// Cap for assets that get *decoded for preview* (same as remote images) —
/// decoding huge images would stall the GTK main thread.
pub const ASSET_MAX_BYTES: u64 = 20 * 1024 * 1024;

/// Sanity cap when copying an asset out of a pack into memory (export,
/// open/save attachment). Guards against decompression bombs: a hostile zip
/// can declare a small size but inflate to gigabytes, so the cap is enforced
/// on the actual decompressed stream, not the header. Streaming from real
/// files (`AssetSource::File`) needs no cap — disk sizes can't lie.
pub const COPY_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Cap for the markdown text entry of a pack.
const TEXT_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Content for a pack entry: bytes already in memory, or a file on disk that
/// is streamed when the pack is written (never held in RAM).
#[derive(Clone)]
pub enum AssetSource {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl AssetSource {
    fn write_into(&self, writer: &mut impl Write) -> io::Result<()> {
        match self {
            AssetSource::Bytes(bytes) => writer.write_all(bytes),
            AssetSource::File(path) => io::copy(&mut File::open(path)?, writer).map(|_| ()),
        }
    }
}

/// Read a file fully, or None when it is missing or larger than `max`.
pub fn read_file_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    if fs::metadata(path).ok()?.len() > max {
        return None;
    }
    fs::read(path).ok()
}

const INFO_JSON: &str = r#"{
  "version": 2,
  "type": "net.daringfireball.markdown",
  "transient": false,
  "creatorIdentifier": "no.oyzmo.Bindle"
}
"#;

/// Location of the markdown inside a pack: some packs nest everything in a
/// `Name.textbundle/` wrapper folder (`prefix`), others sit at the root.
#[derive(Clone)]
pub struct PackInfo {
    pub prefix: String,
    pub text_name: String,
}

pub fn is_bundle(path: &Path) -> bool {
    path.is_dir() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("textbundle"))
}

pub fn is_pack(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("textpack"))
}

fn is_text_name(name: &str) -> bool {
    matches!(name, "text.md" | "text.markdown" | "text.txt")
}

/// Read the markdown of a .textbundle directory: (text, text file name).
pub fn open_bundle(dir: &Path) -> io::Result<(String, String)> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if is_text_name(&name) {
            return Ok((fs::read_to_string(entry.path())?, name));
        }
    }
    Err(io::Error::other("no text.md in the bundle"))
}

/// Read the markdown of a .textpack zip: (text, entry location).
pub fn open_pack(path: &Path) -> io::Result<(String, PackInfo)> {
    let mut zip = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    let names: Vec<String> = zip.file_names().map(String::from).collect();
    let mut best: Option<PackInfo> = None;
    for full in &names {
        let (prefix, base) = match full.rsplit_once('/') {
            Some((p, b)) => (format!("{p}/"), b),
            None => (String::new(), full.as_str()),
        };
        // The text file may sit at the root or inside one wrapper folder;
        // prefer the shallower match.
        if is_text_name(base)
            && full.matches('/').count() <= 1
            && best.as_ref().is_none_or(|b| prefix.len() < b.prefix.len())
        {
            best = Some(PackInfo { prefix, text_name: base.to_string() });
        }
    }
    let info = best.ok_or_else(|| io::Error::other("no text.md in the textpack"))?;
    let mut entry = zip
        .by_name(&format!("{}{}", info.prefix, info.text_name))
        .map_err(io::Error::other)?;
    if entry.size() > TEXT_MAX_BYTES {
        return Err(io::Error::other("the pack's text entry is unreasonably large"));
    }
    let mut text = String::new();
    (&mut entry).take(TEXT_MAX_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > TEXT_MAX_BYTES {
        return Err(io::Error::other("the pack's text entry is unreasonably large"));
    }
    Ok((text, info))
}

/// Read one asset (e.g. "assets/pic.png") from a pack. `max` caps the size —
/// `ASSET_MAX_BYTES` when the bytes will be decoded, `COPY_MAX_BYTES` for
/// plain copying — and is enforced on the decompressed stream, since a
/// hostile zip header can understate the size.
pub fn read_asset(path: &Path, info: &PackInfo, rel: &str, max: u64) -> Option<Vec<u8>> {
    let mut zip = ZipArchive::new(File::open(path).ok()?).ok()?;
    let mut entry = zip.by_name(&format!("{}{}", info.prefix, rel)).ok()?;
    if entry.size() > max {
        return None;
    }
    let mut buf = Vec::with_capacity((entry.size() as usize).min(1 << 20));
    (&mut entry).take(max + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > max {
        return None;
    }
    Some(buf)
}

/// Rewrite a pack with new text and any staged assets. Atomic: unchanged
/// entries are raw-copied into a temp file that then replaces the original,
/// via `doc::replace_via_temp` so a symlinked pack stays a symlink and a
/// private one stays private. Unlike a plain file there is no write-in-place
/// fallback: the new pack is built *from* the old one, so truncating it first
/// would destroy the source mid-copy.
pub fn save_pack(
    path: &Path,
    info: &PackInfo,
    text: &str,
    staged: &HashMap<String, AssetSource>,
) -> io::Result<()> {
    let mut old = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    let result = crate::doc::replace_via_temp(path, |file| {
        let mut writer = ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        let text_full = format!("{}{}", info.prefix, info.text_name);
        let replaced: Vec<String> = std::iter::once(text_full.clone())
            .chain(staged.keys().map(|k| format!("{}{}", info.prefix, k)))
            .collect();
        for i in 0..old.len() {
            let entry = old.by_index_raw(i).map_err(io::Error::other)?;
            if replaced.iter().any(|r| r == entry.name()) {
                continue;
            }
            writer.raw_copy_file(entry).map_err(io::Error::other)?;
        }
        writer.start_file(text_full, opts).map_err(io::Error::other)?;
        writer.write_all(text.as_bytes())?;
        for (rel, source) in staged {
            writer
                .start_file(format!("{}{}", info.prefix, rel), opts)
                .map_err(io::Error::other)?;
            source.write_into(&mut writer)?;
        }
        writer.finish().map_err(io::Error::other)?;
        Ok(())
    });
    match result {
        Ok(()) => Ok(()),
        Err(crate::doc::Replace::Unsupported(e) | crate::doc::Replace::Failed(e)) => Err(e),
    }
}

/// Write a fresh .textpack containing `text` and `assets` (file name, content).
pub fn export_pack(dest: &Path, text: &str, assets: &[(String, AssetSource)]) -> io::Result<()> {
    let stem = dest
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Document".into());
    let prefix = format!("{stem}.textbundle/");
    let tmp = tmp_path(dest);
    let result = (|| {
        let mut writer = ZipWriter::new(File::create(&tmp)?);
        let opts = SimpleFileOptions::default();
        writer.start_file(format!("{prefix}info.json"), opts).map_err(io::Error::other)?;
        writer.write_all(INFO_JSON.as_bytes())?;
        writer.start_file(format!("{prefix}text.md"), opts).map_err(io::Error::other)?;
        writer.write_all(text.as_bytes())?;
        for (name, source) in assets {
            writer
                .start_file(format!("{prefix}assets/{name}"), opts)
                .map_err(io::Error::other)?;
            source.write_into(&mut writer)?;
        }
        writer.finish().map_err(io::Error::other)?;
        Ok(())
    })();
    match result {
        Ok(()) => fs::rename(&tmp, dest),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write a pack attachment to a read-only temp file for viewing (`scope`
/// keeps different packs' attachments apart). Read-only is the enforcement
/// behind "attachments open as read-only copies": most applications will
/// then require Save As instead of silently editing a file that would be
/// thrown away.
pub fn extract_readonly_temp(scope: &str, rel: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment".to_string());
    let dir = std::env::temp_dir().join("bindle-attachments").join(scope);
    fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    if let Ok(meta) = fs::metadata(&path) {
        // A previous extraction left it read-only; unlock to overwrite.
        let mut perms = meta.permissions();
        perms.set_readonly(false);
        let _ = fs::set_permissions(&path, perms);
    }
    fs::write(&path, bytes)?;
    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(&path, perms)?;
    Ok(path)
}

/// `base` with `-n` before its extension: `photo.png` → `photo-2.png`.
pub fn numbered_name(base: &str, n: u32) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{base}-{n}"),
    }
}

/// The first of `name`, `name-2`, `name-3`… that `taken` does not claim.
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut candidate = name.to_string();
    let mut n = 2;
    while taken(&candidate) {
        candidate = numbered_name(name, n);
        n += 1;
    }
    candidate
}

/// Whether two paths name the same file (device and inode), however they
/// are spelled. Copying a file onto itself truncates it to nothing.
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Copy `src` into the directory `assets` and return the file name it has
/// there. Never overwrites: a different file already holding the name keeps
/// it — other references may point at it — and the copy gets the next free
/// `-2`, `-3`… name. A file that already *is* in `assets` is used where it
/// is rather than copied onto itself, which would empty it.
pub fn copy_into_assets(src: &Path, assets: &Path) -> io::Result<String> {
    let base = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    fs::create_dir_all(assets)?;
    let mut name = base.clone();
    let mut n = 2;
    loop {
        let target = assets.join(&name);
        if same_file(src, &target) {
            return Ok(name);
        }
        if fs::symlink_metadata(&target).is_err() {
            break;
        }
        name = numbered_name(&base, n);
        n += 1;
    }
    let target = assets.join(&name);
    // create_new: even a file that appeared since the check is never clobbered.
    let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
    if let Err(err) = File::open(src).and_then(|mut input| io::copy(&mut input, &mut out)) {
        drop(out);
        let _ = fs::remove_file(&target);
        return Err(err);
    }
    Ok(name)
}

/// Every entry name in a zip, prefix included.
pub fn pack_entry_names(path: &Path) -> io::Result<Vec<String>> {
    let zip = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    Ok(zip.file_names().map(String::from).collect())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Distinct local file destinations referenced by `markdown` — images and
/// link targets — in order. Web URLs, anchors and the like are skipped;
/// whether a destination actually resolves to a file is the caller's problem.
pub fn local_asset_dests(markdown: &str) -> Vec<String> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut out: Vec<String> = Vec::new();
    for event in Parser::new_ext(markdown, options) {
        let dest = match event {
            Event::Start(Tag::Image { dest_url, .. }) => dest_url.to_string(),
            Event::Start(Tag::Link { dest_url, .. }) => dest_url.to_string(),
            _ => continue,
        };
        let skip = dest.is_empty()
            || dest.contains("://")
            || dest.starts_with('#')
            || dest.starts_with("data:")
            || dest.starts_with("mailto:")
            || dest.starts_with("tel:");
        if !skip && !out.contains(&dest) {
            out.push(dest);
        }
    }
    out
}

/// Decode percent-escapes (%20, %28, UTF-8 sequences) in a markdown
/// destination; returns the input unchanged when the result is not UTF-8.
pub fn decode_dest(dest: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = dest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| dest.to_string())
}

fn wrap_target(target: &str) -> String {
    if target.contains(' ') { format!("<{target}>") } else { target.to_string() }
}

/// Point image and link references at `assets/<name>`: `renames` maps an
/// original destination to the asset's file name inside the pack. Only the
/// destination spans the parser identifies are rewritten — inline references
/// (titled or not) plus `[label]: dest` definition lines — so lookalike text
/// in code blocks is never touched.
pub fn rewrite_asset_dests(markdown: &str, renames: &[(String, String)]) -> String {
    if renames.is_empty() {
        return markdown.to_string();
    }
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    // Byte spans of destinations as written, with their replacements.
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        let dest = match event {
            Event::Start(Tag::Image { dest_url, .. }) => dest_url.to_string(),
            Event::Start(Tag::Link { dest_url, .. }) => dest_url.to_string(),
            _ => continue,
        };
        let Some((_, name)) = renames.iter().find(|(old, _)| *old == dest) else {
            continue;
        };
        let Some(src) = markdown.get(range.clone()) else {
            continue;
        };
        // The destination appears as written: literal or %20-encoded.
        // rfind: the dest sits at the end of `[text](dest)`, so a lookalike
        // in the link text is not matched.
        let encoded = dest.replace(' ', "%20");
        let (pos, len) = match src.rfind(dest.as_str()) {
            Some(p) => (p, dest.len()),
            None => match src.rfind(encoded.as_str()) {
                Some(p) => (p, encoded.len()),
                None => continue, // reference-style usage: definition line below
            },
        };
        let mut start = range.start + pos;
        let mut end = start + len;
        if markdown.as_bytes().get(start.wrapping_sub(1)) == Some(&b'<')
            && markdown.as_bytes().get(end) == Some(&b'>')
        {
            start -= 1;
            end += 1;
        }
        spans.push((start, end, wrap_target(&format!("assets/{name}"))));
    }
    // Reference definitions ("[label]: dest") have no parser event; scan lines.
    let mut pos = 0;
    for line in markdown.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        let indent = content.len() - trimmed.len();
        if trimmed.starts_with('[') {
            if let Some(colon) = trimmed.find("]:") {
                let after = &trimmed[colon + 2..];
                let lead = after.len() - after.trim_start().len();
                let rest = after.trim_start();
                let token_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
                let token = &rest[..token_len];
                let plain =
                    token.strip_prefix('<').and_then(|t| t.strip_suffix('>')).unwrap_or(token);
                let decoded = decode_dest(plain);
                if let Some((_, name)) =
                    renames.iter().find(|(old, _)| *old == plain || *old == decoded)
                {
                    let start = pos + indent + colon + 2 + lead;
                    spans.push((start, start + token_len, wrap_target(&format!("assets/{name}"))));
                }
            }
        }
        pos += line.len();
    }
    spans.sort_by(|a, b| b.0.cmp(&a.0));
    spans.dedup_by(|a, b| a.0 == b.0);
    let mut out = markdown.to_string();
    for (start, end, rep) in spans {
        out.replace_range(start..end, &rep);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("bindle-assets-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn numbering_keeps_the_extension() {
        assert_eq!(numbered_name("photo.png", 2), "photo-2.png");
        assert_eq!(numbered_name("README", 3), "README-3");
        assert_eq!(numbered_name(".hidden", 2), ".hidden-2");
        assert_eq!(unique_name("a.png", |n| n == "a.png" || n == "a-2.png"), "a-3.png");
    }

    #[test]
    fn a_file_already_in_assets_is_not_copied_onto_itself() {
        let dir = scratch("self");
        let assets = dir.join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("pic.png"), b"image bytes").unwrap();
        let name = copy_into_assets(&assets.join("pic.png"), &assets).unwrap();
        assert_eq!(name, "pic.png");
        // fs::copy onto itself would have left this empty.
        assert_eq!(fs::read(assets.join("pic.png")).unwrap(), b"image bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_same_named_different_file_gets_a_new_name() {
        let dir = scratch("clash");
        let assets = dir.join("assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("pic.png"), b"original").unwrap();
        fs::create_dir_all(dir.join("elsewhere")).unwrap();
        fs::write(dir.join("elsewhere/pic.png"), b"newcomer").unwrap();
        let name = copy_into_assets(&dir.join("elsewhere/pic.png"), &assets).unwrap();
        assert_eq!(name, "pic-2.png");
        assert_eq!(fs::read(assets.join("pic.png")).unwrap(), b"original");
        assert_eq!(fs::read(assets.join("pic-2.png")).unwrap(), b"newcomer");
        let _ = fs::remove_dir_all(&dir);
    }
}
