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
    /// Every entry name in the zip, prefix included, as of opening or the
    /// last save — what a newly staged asset must not collide with. Kept
    /// here so staging never has to re-read the pack on the main loop.
    pub entries: Vec<String>,
}

pub fn is_bundle(path: &Path) -> bool {
    path.is_dir() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("textbundle"))
}

pub fn is_pack(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("textpack"))
}

/// The text file's possible names, in order of preference.
const TEXT_NAMES: [&str; 3] = ["text.md", "text.markdown", "text.txt"];

fn is_text_name(name: &str) -> bool {
    TEXT_NAMES.contains(&name)
}

/// Read the markdown of a .textbundle directory: (text, text file name,
/// legacy encoding). Read like a plain file (`doc::read_text`): a bundle
/// made on Windows has CRLF lines or a Windows-1252 text file just as often.
///
/// With more than one candidate the order of `TEXT_NAMES` decides, not the
/// directory listing's, and only a regular file counts.
pub fn open_bundle(dir: &Path) -> io::Result<(String, String, Option<&'static str>)> {
    // Listed first so a missing bundle stays `NotFound` (ui.rs reads that
    // as an unmounted share).
    let names: Vec<String> =
        fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    for name in TEXT_NAMES {
        let path = dir.join(name);
        if names.iter().any(|n| n == name) && path.is_file() {
            let (text, legacy) = crate::doc::read_text(&path)?;
            return Ok((text, name.to_string(), legacy));
        }
    }
    Err(io::Error::other("no text.md in the bundle"))
}

/// Read the markdown of a .textpack zip: (text, entry location, legacy
/// encoding). Decoded like a plain file (`doc::decode_text`): a pack made
/// on Windows has CRLF lines or Windows-1252 text as often as a bundle.
pub fn open_pack(path: &Path) -> io::Result<(String, PackInfo, Option<&'static str>)> {
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
            best = Some(PackInfo { prefix, text_name: base.to_string(), entries: Vec::new() });
        }
    }
    let mut info = best.ok_or_else(|| io::Error::other("no text.md in the textpack"))?;
    info.entries = names;
    let mut entry = zip
        .by_name(&format!("{}{}", info.prefix, info.text_name))
        .map_err(io::Error::other)?;
    if entry.size() > TEXT_MAX_BYTES {
        return Err(io::Error::other("the pack's text entry is unreasonably large"));
    }
    let mut bytes = Vec::new();
    (&mut entry).take(TEXT_MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > TEXT_MAX_BYTES {
        return Err(io::Error::other("the pack's text entry is unreasonably large"));
    }
    let (text, legacy) = crate::doc::decode_text(bytes, crate::doc::DocKind::of(Path::new(&info.text_name)));
    Ok((text, info, legacy))
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
/// would destroy the source mid-copy. Returns the entry names now in it.
pub fn save_pack(
    path: &Path,
    info: &PackInfo,
    text: &str,
    staged: &HashMap<String, AssetSource>,
) -> io::Result<Vec<String>> {
    let mut old = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    let mut names: Vec<String> = Vec::new();
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
            names.push(entry.name().to_string());
            writer.raw_copy_file(entry).map_err(io::Error::other)?;
        }
        names.extend(replaced);
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
        Ok(()) => Ok(names),
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
    // Through `replace_via_temp` like pack saves: overwriting an existing
    // pack keeps a symlink a symlink and a private file private.
    let result = crate::doc::replace_via_temp(dest, |file| {
        let mut writer = ZipWriter::new(file);
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
    });
    match result {
        Ok(()) => Ok(()),
        Err(crate::doc::Replace::Unsupported(e) | crate::doc::Replace::Failed(e)) => Err(e),
    }
}

/// Write a pack attachment to a read-only temp file for viewing (`scope`
/// keeps different packs' attachments apart). Read-only is the enforcement
/// behind "attachments open as read-only copies": most applications will
/// then require Save As instead of silently editing a file that would be
/// thrown away.
///
/// The copies live in the user's own runtime directory, not the shared
/// `/tmp`: there another local user could plant a symlink under the
/// predictable name and have the write land wherever it points.
pub fn extract_readonly_temp(scope: &str, rel: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    extract_readonly_into(&gtk4::glib::user_runtime_dir(), scope, rel, bytes)
}

fn extract_readonly_into(root: &Path, scope: &str, rel: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let name = Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment".to_string());
    let dir = root.join("bindle-attachments").join(scope);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let path = dir.join(name);
    // A previous extraction: replace it rather than write through it.
    match fs::remove_file(&path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    // create_new never follows a link; the read-only mode applies to later
    // opens, so this handle can still write.
    let mut out = fs::OpenOptions::new().write(true).create_new(true).mode(0o400).open(&path)?;
    if let Err(err) = out.write_all(bytes) {
        drop(out);
        let _ = fs::remove_file(&path);
        return Err(err);
    }
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

/// Write `bytes` as a new file named `name` (renumbered past existing ones)
/// inside the `assets` directory, creating it if needed; returns the stored
/// name. Nothing already there is ever overwritten.
pub fn write_into_assets(name: &str, bytes: &[u8], assets: &Path) -> io::Result<String> {
    fs::create_dir_all(assets)?;
    let stored = unique_name(name, |candidate| fs::symlink_metadata(assets.join(candidate)).is_ok());
    let target = assets.join(&stored);
    let mut out = fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
    if let Err(err) = out.write_all(bytes) {
        drop(out);
        let _ = fs::remove_file(&target);
        return Err(err);
    }
    Ok(stored)
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

/// `s` with markdown's backslash escapes (before ASCII punctuation) removed.
fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Byte length of the link destination `s` starts with, as markdown reads
/// it: `<…>` through the closing bracket, or a bare run ending at whitespace
/// or an unbalanced `)`, backslash escapes included. Only ASCII bytes end
/// it, so the length always falls on a char boundary.
fn dest_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'<') {
        i = 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'>' => return Some(i + 1),
                b'<' | b'\n' => return None,
                _ => i += 1,
            }
        }
        return None;
    }
    let mut depth = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => i += 2,
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' if depth == 0 => break,
            b')' => {
                depth -= 1;
                i += 1;
            }
            c if c.is_ascii_whitespace() || c.is_ascii_control() => break,
            _ => i += 1,
        }
    }
    (i > 0).then_some(i)
}

/// Whether `s`, the text after a destination, closes the link: optional
/// whitespace, an optional `"…"`/`'…'`/`(…)` title, whitespace, then `)` as
/// the last character.
fn closes_link(s: &str) -> bool {
    let rest = s.trim_start();
    let b = rest.as_bytes();
    let rest = match b.first() {
        Some(&open @ (b'"' | b'\'' | b'(')) => {
            let close = if open == b'(' { b')' } else { open };
            let mut i = 1;
            loop {
                match b.get(i) {
                    None => return false,
                    Some(b'\\') => i += 2,
                    Some(&c) if c == close => break,
                    _ => i += 1,
                }
            }
            rest[i + 1..].trim_start()
        }
        _ => rest,
    };
    rest == ")"
}

/// Byte range of the destination inside `src`, the whole source of one
/// inline link or image: after the leftmost `](` from which a destination,
/// an optional title and the closing `)` make up exactly the rest. Link
/// text (a nested image) and titles may hold `](` too; neither parses so.
fn inline_dest_range(src: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(p) = src[from..].find("](") {
        let open = from + p + 2;
        let tail = &src[open..];
        let start = open + tail.len() - tail.trim_start().len();
        if let Some(len) = dest_len(&src[start..]) {
            if closes_link(&src[start + len..]) {
                return Some((start, start + len));
            }
        }
        from = open;
    }
    None
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
    // Code blocks, so the definition scan below can leave their text alone.
    let mut code: Vec<std::ops::Range<usize>> = Vec::new();
    for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
        let dest = match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code.push(range);
                continue;
            }
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
        // Only the inline form `[text](dest)` holds its destination here.
        // A reference link (`[text][label]`, `[label]`, `[label][]`) is
        // rewritten at its definition line below; its label must stay as
        // written, or it would stop matching that definition — and in the
        // shortcut form the label is the only place the name appears.
        if !src.ends_with(')') {
            continue;
        }
        // Located by parsing, not by searching for `dest`: the parser hands
        // over the destination unescaped (`a\_b.png` arrives as `a_b.png`),
        // which the source need not contain as written.
        let Some((start, end)) = inline_dest_range(src) else {
            continue;
        };
        spans.push((range.start + start, range.start + end, wrap_target(&format!("assets/{name}"))));
    }
    // Reference definitions ("[label]: dest") have no parser event; scan lines.
    let mut pos = 0;
    for line in markdown.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        let indent = content.len() - trimmed.len();
        // Four spaces is an indented code block; a fenced one is in `code`.
        let in_code = indent >= 4 || code.iter().any(|r| r.contains(&pos));
        if !in_code && trimmed.starts_with('[') {
            if let Some(colon) = trimmed.find("]:") {
                let after = &trimmed[colon + 2..];
                let lead = after.len() - after.trim_start().len();
                let rest = after.trim_start();
                // `<my file.pdf>` is one destination, spaces and all.
                let token_len = dest_len(rest).unwrap_or(0);
                let token = &rest[..token_len];
                let plain =
                    token.strip_prefix('<').and_then(|t| t.strip_suffix('>')).unwrap_or(token);
                let decoded = decode_dest(plain);
                let unescaped = unescape_backslashes(plain);
                if let Some((_, name)) = renames
                    .iter()
                    .find(|(old, _)| !plain.is_empty() && (*old == plain || *old == decoded || *old == unescaped))
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

    fn pdf(dest: &str) -> Vec<(String, String)> {
        vec![(dest.to_string(), "report.pdf".to_string())]
    }

    #[test]
    fn a_shortcut_reference_keeps_its_label() {
        let md = "See [report.pdf] and [this][report.pdf].\n\n[report.pdf]: report.pdf\n";
        assert_eq!(
            rewrite_asset_dests(md, &pdf("report.pdf")),
            "See [report.pdf] and [this][report.pdf].\n\n[report.pdf]: assets/report.pdf\n"
        );
    }

    #[test]
    fn inline_dests_are_rewritten_but_not_titles_or_link_text() {
        let md = "[report.pdf](report.pdf \"see report.pdf\") ![a](<my report.pdf>)\n";
        let renames = [pdf("report.pdf"), pdf("my report.pdf")].concat();
        assert_eq!(
            rewrite_asset_dests(md, &renames),
            "[report.pdf](assets/report.pdf \"see report.pdf\") ![a](assets/report.pdf)\n"
        );
    }

    #[test]
    fn escaped_and_bracketed_dests_are_found() {
        let md = "![](a\\_b.png) [t](<x](y.pdf> \"t](z\")\n\n[d]: <my report.pdf> \"T\"\n[e]: c\\_d.pdf\n";
        let renames = [
            ("a_b.png".to_string(), "a_b.png".to_string()),
            ("x](y.pdf".to_string(), "y.pdf".to_string()),
            ("my report.pdf".to_string(), "report.pdf".to_string()),
            ("c_d.pdf".to_string(), "c_d.pdf".to_string()),
        ];
        assert_eq!(
            rewrite_asset_dests(md, &renames),
            "![](assets/a_b.png) [t](assets/y.pdf \"t](z\")\n\n[d]: assets/report.pdf \"T\"\n[e]: assets/c_d.pdf\n"
        );
        // A nested image's `](` is not the outer link's.
        assert_eq!(inline_dest_range("[![i](a.png)](b.pdf)"), Some((14, 19)));
    }

    #[test]
    fn definitions_inside_code_are_left_alone() {
        let md = "```\n[x]: report.pdf\n```\n\n    [y]: report.pdf\n\n[z]: report.pdf\n";
        assert_eq!(
            rewrite_asset_dests(md, &pdf("report.pdf")),
            "```\n[x]: report.pdf\n```\n\n    [y]: report.pdf\n\n[z]: assets/report.pdf\n"
        );
    }

    #[test]
    fn extraction_replaces_a_planted_symlink_instead_of_following_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("extract");
        let victim = dir.join("victim.txt");
        fs::write(&victim, b"keep me").unwrap();
        let scope_dir = dir.join("bindle-attachments").join("s");
        fs::create_dir_all(&scope_dir).unwrap();
        std::os::unix::fs::symlink(&victim, scope_dir.join("a.pdf")).unwrap();
        let path = extract_readonly_into(&dir, "s", "assets/a.pdf", b"attachment").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
        assert_eq!(fs::read(&path).unwrap(), b"attachment");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o400);
        // A second extraction over the read-only copy still works.
        extract_readonly_into(&dir, "s", "assets/a.pdf", b"again").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"again");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pasted_bytes_never_overwrite_an_existing_asset() {
        let dir = scratch("paste");
        let assets = dir.join("assets");
        assert_eq!(write_into_assets("shot.png", b"one", &assets).unwrap(), "shot.png");
        let second = write_into_assets("shot.png", b"two", &assets).unwrap();
        assert_ne!(second, "shot.png");
        assert_eq!(fs::read(assets.join("shot.png")).unwrap(), b"one");
        assert_eq!(fs::read(assets.join(&second)).unwrap(), b"two");
        let _ = fs::remove_dir_all(&dir);
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
