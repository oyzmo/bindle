//! Document kinds beyond markdown: which formats the editor understands and
//! how to read them from disk (encoding fallbacks, newline normalization).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gtk4::glib;

/// What the open file contains, decided by its extension. Everything
/// unknown is treated as markdown (the previous behavior for any file).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocKind {
    Markdown,
    Text,
    Nfo,
    Json,
    Yaml,
    Toml,
    Shell,
    Xml,
}

impl DocKind {
    pub fn of(path: &Path) -> DocKind {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("txt" | "text" | "log" | "csv" | "tsv") => DocKind::Text,
            // FILE_ID.DIZ is the same DOS-era art in the same encoding.
            Some("nfo" | "diz") => DocKind::Nfo,
            Some("json") => DocKind::Json,
            Some("yaml" | "yml") => DocKind::Yaml,
            // INI is close enough to TOML — sections, key = value, comments —
            // that the TOML scanner reads it usefully.
            Some("toml" | "ini" | "cfg" | "conf") => DocKind::Toml,
            Some("sh" | "bash" | "zsh" | "ksh") => DocKind::Shell,
            Some("xml" | "xsd" | "xsl" | "xslt") => DocKind::Xml,
            _ => DocKind::Markdown,
        }
    }

    pub fn is_markdown(self) -> bool {
        self == DocKind::Markdown
    }

    /// Smart Enter/Backspace (list continuation) makes sense here.
    /// YAML lists use the same "- " markers as markdown; JSON/TOML/NFO
    /// lines starting with "-" mean something else entirely.
    pub fn smart_edit(self) -> bool {
        matches!(self, DocKind::Markdown | DocKind::Yaml | DocKind::Text)
    }

    /// Long lines wrap — except in NFO/DIZ art, which depends on every line
    /// keeping its exact width. Applies to the editor as well as the preview:
    /// with no rendered preview for these kinds (see [`uses_panels`]), the
    /// editor is the only place the art is ever seen.
    ///
    /// [`uses_panels`]: DocKind::uses_panels
    pub fn wrap_lines(self) -> bool {
        self != DocKind::Nfo
    }

    /// Whether the Insert and Preview side panels mean anything here.
    ///
    /// Markdown is the only kind with a *rendered* preview: every other one
    /// previews as a verbatim copy of the editor's own text, which costs a
    /// full re-render on every keystroke to show what is already on screen.
    /// The snippets are markdown syntax, so the Insert list is dead weight
    /// too. Both panels (and view mode) therefore switch off for non-markdown
    /// documents — as a runtime override, leaving the user's own toggles
    /// untouched for the next markdown file.
    pub fn uses_panels(self) -> bool {
        self.is_markdown()
    }
}

/// Read a document as text: UTF-8, falling back to CP437 for .nfo files
/// (the traditional encoding of their box-drawing art) and Windows-1252 for
/// anything else. CRLF/CR line endings are normalized to LF; saving writes
/// UTF-8 with LF back. The second value names the legacy encoding used, if
/// any, so the caller can say that saving will convert the file.
///
/// Never lossy UTF-8: that turns every non-ASCII byte of an old Latin-1
/// file into U+FFFD, and the next save writes those over the original for
/// good. Both fallbacks map every byte to a character, so nothing is lost.
pub fn read_text(path: &Path) -> io::Result<(String, Option<&'static str>)> {
    let bytes = fs::read(path)?;
    let (text, legacy) = match String::from_utf8(bytes) {
        Ok(s) => (s, None),
        Err(err) => {
            let bytes = err.into_bytes();
            if DocKind::of(path) == DocKind::Nfo {
                (cp437(&bytes), Some("CP437"))
            } else {
                (cp1252(&bytes), Some("Windows-1252"))
            }
        }
    };
    if text.contains('\r') {
        Ok((text.replace("\r\n", "\n").replace('\r', "\n"), legacy))
    } else {
        Ok((text, legacy))
    }
}

/// Replace `path` by writing a hidden sibling temp file with `write`,
/// flushing it and renaming it over the original. Symlinks are followed, so
/// the link keeps pointing at the updated file, and the original's
/// permissions are put on the temp file *before* any data goes in — created
/// owner-only and only then widened, so a private document is never
/// readable by others while it is being written. A chmod the filesystem
/// refuses leaves it owner-only: narrower than before, never wider.
///
/// `Unsupported` means the location refused the temp file or the rename
/// (some gvfs backends), and the original is untouched; `Failed` is an error
/// writing the temp file, such as a full disk.
pub fn replace_via_temp(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> Result<(), Replace> {
    use std::os::unix::fs::OpenOptionsExt;
    let target = resolve(path);
    let Some(name) = target.file_name() else {
        return Err(Replace::Unsupported(io::Error::other("not a file path")));
    };
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = target.with_file_name(tmp_name);
    let original = fs::metadata(&target).ok().map(|m| m.permissions());
    // A stale temp from a crashed save would keep its old mode: start fresh.
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    if original.is_some() {
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(Replace::Unsupported)?;
    if let Some(perms) = original {
        let _ = file.set_permissions(perms);
    }
    let written = write(&mut file).and_then(|_| match file.sync_all() {
        // FUSE filesystems may not implement fsync; the rename still helps.
        Err(err) if err.kind() == io::ErrorKind::Unsupported => Ok(()),
        other => other,
    });
    drop(file);
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(Replace::Failed(err));
    }
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(Replace::Unsupported(err));
    }
    Ok(())
}

/// Why `replace_via_temp` failed; see there.
pub enum Replace {
    Unsupported(io::Error),
    Failed(io::Error),
}

/// `path` with symlinks resolved, or as given when it does not exist yet.
fn resolve(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Replace `path` with `data` without ever leaving it half-written: write a
/// hidden sibling temp file, flush it, then rename it over the original. A
/// plain `fs::write` truncates first, so a write failing partway — a full
/// disk, a network share dropping — destroyed the very file being saved.
///
/// Symlinks and permissions as in `replace_via_temp`. A directory that
/// refuses the temp file, or a backend that cannot rename (some gvfs ones),
/// falls back to writing in place — no worse than before. A failure
/// *writing* the temp file does not fall back: that is the full-disk case,
/// where writing in place would truncate the original and then fail the
/// same way.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    match replace_via_temp(path, |file| file.write_all(data)) {
        Ok(()) => Ok(()),
        Err(Replace::Unsupported(_)) => fs::write(resolve(path), data),
        Err(Replace::Failed(err)) => Err(err),
    }
}

/// What a file looked like on disk when the document was read or written:
/// modification time and size. A different stamp later means another
/// program changed the file under us. Size is in it because a coarse mtime
/// (some network shares round to seconds) can miss a quick rewrite.
pub type DiskStamp = (std::time::SystemTime, u64);

/// The current stamp of `path`, following symlinks as reads and writes do.
/// `None` when it can't be stat'ed — gone, or the share is down. Blocks on
/// an unreachable mount, so the window calls it from a worker thread except
/// right after it has written the file itself.
pub fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The gvfs FUSE root, under which gvfsd-fuse gives every mounted share a
/// directory named after its mount spec.
fn gvfs_root() -> PathBuf {
    glib::user_runtime_dir().join("gvfs")
}

/// The share a path lives on, as a URI that can be mounted again — `None`
/// for anything that is not on a gvfs share.
///
/// Paths under the FUSE root exist only while their share is mounted, so a
/// remembered one (Open Recent, the restored last file) fails with a bare
/// "No such file or directory" until something mounts the share — which is
/// why opening it in Files first makes it work here. GIO is no help: the
/// path is an ordinary local one to it, so `mount_enclosing_volume` on it
/// fails. The directory name *is* the mount spec though, so this turns it
/// back into the URI it was mounted from; mounting that recreates the very
/// same directory, and with it the path.
pub fn gvfs_share_uri(path: &Path) -> Option<String> {
    let dir = path.strip_prefix(gvfs_root()).ok()?.components().next()?;
    mount_spec_uri(&dir.as_os_str().to_string_lossy())
}

/// Wait for a just-mounted share's FUSE directory to turn up. GIO reports a
/// mount as soon as the daemon registers it, but gvfsd-fuse hears about it
/// over D-Bus a moment later — reads in that window fail with ENOENT or,
/// worse, EIO. Blocks, so this belongs on a worker thread.
pub fn wait_for_path(path: &Path) {
    for _ in 0..30 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `type:key=value,key=value` (a mount spec, as gvfsd-fuse names its
/// directories) → the URI it was mounted from. Only the share's root needs
/// to come out right; the rest of the path is appended by the caller's
/// existing path, so keys that merely narrow the location down are dropped.
fn mount_spec_uri(dir: &str) -> Option<String> {
    let (kind, spec) = dir.split_once(':')?;
    let fields: Vec<(&str, &str)> = spec.split(',').filter_map(|f| f.split_once('=')).collect();
    let field = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let scheme = match kind {
        // One WebDAV backend, with TLS flagged in the spec instead of the type.
        "dav" | "davs" => {
            if field("ssl") == Some("true") {
                "davs"
            } else {
                "dav"
            }
        }
        // The browse/server/share variants share one URI scheme.
        "smb-share" | "smb-server" | "smb-browse" => "smb",
        "afp-volume" | "afp-server" => "afp",
        other => other,
    };
    let host = field("host").or_else(|| field("server"))?;
    let mut uri = format!("{scheme}://");
    if let Some(user) = field("user") {
        uri.push_str(user);
        uri.push('@');
    }
    uri.push_str(host);
    if let Some(port) = field("port") {
        uri.push(':');
        uri.push_str(port);
    }
    uri.push('/');
    // A prefix means the mount was rooted below the server's own root; the
    // share (SMB) or volume (AFP) name plays the same part.
    if let Some(prefix) = field("prefix") {
        let raw = glib::Uri::unescape_string(prefix, None)?;
        uri.push_str(glib::Uri::escape_string(&raw, Some("/"), true).trim_start_matches('/'));
    } else if let Some(name) = field("share").or_else(|| field("volume")) {
        uri.push_str(name);
    }
    Some(uri)
}

/// Decode CP437 (DOS) bytes; the high half carries the box-drawing and
/// shading characters classic NFO art is made of.
fn cp437(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                CP437_HIGH[(b - 0x80) as usize]
            }
        })
        .collect()
}

/// Windows-1252, the usual encoding of old Western text files (a superset
/// of Latin-1's printable range). Its five unassigned bytes map to the C1
/// controls of the same value, as browsers do, so every byte survives.
fn cp1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252_C1[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

#[rustfmt::skip]
const CP1252_C1: [char; 32] = [
    '€','\u{81}','‚','ƒ','„','…','†','‡','ˆ','‰','Š','‹','Œ','\u{8D}','Ž','\u{8F}',
    '\u{90}','‘','’','“','”','•','–','—','˜','™','š','›','œ','\u{9D}','ž','Ÿ',
];

#[rustfmt::skip]
const CP437_HIGH: [char; 128] = [
    'Ç','ü','é','â','ä','à','å','ç','ê','ë','è','ï','î','ì','Ä','Å',
    'É','æ','Æ','ô','ö','ò','û','ù','ÿ','Ö','Ü','¢','£','¥','₧','ƒ',
    'á','í','ó','ú','ñ','Ñ','ª','º','¿','⌐','¬','½','¼','¡','«','»',
    '░','▒','▓','│','┤','╡','╢','╖','╕','╣','║','╗','╝','╜','╛','┐',
    '└','┴','┬','├','─','┼','╞','╟','╚','╔','╩','╦','╠','═','╬','╧',
    '╨','╤','╥','╙','╘','╒','╓','╫','╪','┘','┌','█','▄','▌','▐','▀',
    'α','ß','Γ','π','Σ','σ','µ','τ','Φ','Θ','Ω','δ','∞','φ','ε','∩',
    '≡','±','≥','≤','⌠','⌡','÷','≈','°','∙','·','√','ⁿ','²','■','\u{00A0}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_by_extension() {
        assert!(DocKind::of(Path::new("a.md")).is_markdown());
        assert!(DocKind::of(Path::new("a")).is_markdown());
        assert_eq!(DocKind::of(Path::new("a.NFO")), DocKind::Nfo);
        assert_eq!(DocKind::of(Path::new("a.yml")), DocKind::Yaml);
        assert_eq!(DocKind::of(Path::new("a.json")), DocKind::Json);
        assert_eq!(DocKind::of(Path::new("a.toml")), DocKind::Toml);
        assert_eq!(DocKind::of(Path::new("a.txt")), DocKind::Text);
        assert_eq!(DocKind::of(Path::new("a.log")), DocKind::Text);
        assert_eq!(DocKind::of(Path::new("file_id.diz")), DocKind::Nfo);
        assert_eq!(DocKind::of(Path::new("a.ini")), DocKind::Toml);
        assert_eq!(DocKind::of(Path::new("a.sh")), DocKind::Shell);
        assert_eq!(DocKind::of(Path::new("a.BASH")), DocKind::Shell);
        assert_eq!(DocKind::of(Path::new("a.xml")), DocKind::Xml);
        assert_eq!(DocKind::of(Path::new("a.xslt")), DocKind::Xml);
    }

    #[test]
    fn only_markdown_uses_the_side_panels() {
        assert!(DocKind::Markdown.uses_panels());
        for kind in [DocKind::Text, DocKind::Nfo, DocKind::Json, DocKind::Shell, DocKind::Xml] {
            assert!(!kind.uses_panels(), "{kind:?}");
        }
        // Art keeps its line widths; everything else wraps.
        assert!(!DocKind::Nfo.wrap_lines());
        assert!(DocKind::Xml.wrap_lines());
    }

    #[test]
    fn mount_spec_to_uri() {
        // Verified against gvfs: mounting each URI recreates this directory.
        assert_eq!(
            mount_spec_uri("dav:host=localhost,port=9843,ssl=false").as_deref(),
            Some("dav://localhost:9843/")
        );
        assert_eq!(
            mount_spec_uri("dav:host=cloud.example,ssl=true,prefix=%2Fremote.php%2Fwebdav")
                .as_deref(),
            Some("davs://cloud.example/remote.php/webdav")
        );
        assert_eq!(
            mount_spec_uri("sftp:host=mycastle.attoxlen,port=8010").as_deref(),
            Some("sftp://mycastle.attoxlen:8010/")
        );
        assert_eq!(
            mount_spec_uri("smb-share:server=nas,share=media,user=bob").as_deref(),
            Some("smb://bob@nas/media")
        );
        // Not a mount spec at all.
        assert_eq!(mount_spec_uri("Documents"), None);
        assert_eq!(mount_spec_uri("mtp:no-host=1"), None);
    }

    #[test]
    fn share_uri_only_under_the_fuse_root() {
        assert_eq!(gvfs_share_uri(Path::new("/home/me/notes.md")), None);
        let path = gvfs_root().join("sftp:host=nas/notes/plan.md");
        assert_eq!(gvfs_share_uri(&path).as_deref(), Some("sftp://nas/"));
    }

    #[test]
    fn atomic_write_replaces_through_a_symlink_and_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bindle-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.md");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(fs::metadata(&real).unwrap().permissions().mode() & 0o777, 0o640);
        // No temp file left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp437_box_art() {
        assert_eq!(cp437(&[0xC9, 0xCD, 0xBB]), "╔═╗");
        assert_eq!(cp437(b"plain ascii"), "plain ascii");
    }

    #[test]
    fn non_utf8_text_decodes_as_cp1252_without_loss() {
        // "blåbær €5" in Windows-1252, plus an unassigned byte.
        let bytes = b"bl\xE5b\xE6r \x805 \x81";
        assert_eq!(cp1252(bytes), "blåbær €5 \u{81}");
        let dir = std::env::temp_dir().join(format!("bindle-cp1252-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        fs::write(&path, bytes).unwrap();
        let (text, legacy) = read_text(&path).unwrap();
        assert_eq!(text, "blåbær €5 \u{81}");
        assert_eq!(legacy, Some("Windows-1252"));
        assert!(!text.contains('\u{FFFD}'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atomic_write_never_widens_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bindle-private-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.md");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let result = replace_via_temp(&path, |file| {
            // Mid-write: the temp file already holds data and must be private.
            use std::io::Write;
            file.write_all(b"new")?;
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
