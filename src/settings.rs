use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gtk4::glib;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    /// Position of the divider between the sidebar and the editor.
    pub hpaned: i32,
    /// Position of the divider between the element list and the preview.
    pub vpaned: i32,
    /// Position of the divider between the Files panel and the panels below.
    pub files_paned: i32,
    pub last_file: Option<PathBuf>,
    /// Remote image policy: "ask" (per document), "always" or "never".
    pub remote_images: String,
    /// Color scheme: "system", "light" or "dark".
    pub color_scheme: String,
    /// Colour palette: "bindle" (the app's own) or "system" (stock Adwaita).
    /// Orthogonal to `color_scheme`, which picks light or dark within it.
    pub theme: String,
    /// Start in view mode (preview as main pane) when opening existing files.
    pub open_in_view: bool,
    /// Whether the Files panel (recent documents) is shown in the sidebar.
    pub show_files: bool,
    /// Whether the Insert snippet panel is shown in the sidebar.
    pub show_insert: bool,
    /// Whether the preview panel is shown in the sidebar. Ignored in view
    /// mode, where the sidebar's bottom slot holds the editor instead.
    pub show_preview: bool,
    /// Horizontal layout: editor on top, insert + preview side by side below.
    pub horizontal: bool,
    /// Reopen the last document at launch; false starts with a blank one.
    pub open_last: bool,
    /// Launch as a new instance instead of activating the running one.
    pub multiple_instances: bool,
    /// Explain the read-only pack-attachment model before the first open.
    pub show_attachment_note: bool,
    /// Preview scrolls to follow the editor cursor; false leaves it still.
    pub follow_cursor: bool,
    /// Keyword highlighting in the editor (markdown, JSON, YAML, TOML).
    pub keyword_highlight: bool,
    /// Line numbers in the editor gutter.
    pub line_numbers: bool,
    /// Source line numbers in the preview's gutter.
    pub preview_line_numbers: bool,
    /// Long lines wrap in the editor (NFO/DIZ art never does).
    pub word_wrap: bool,
    /// Editor font size: "small", "normal", "large" or "xlarge".
    pub editor_font: String,
    /// Preview font size: "small", "normal", "large" or "xlarge".
    pub preview_font: String,
    /// Remember per-document state (collapsed sections, view mode) across
    /// sessions. JSON key kept from when it covered folds only.
    pub remember_folds: bool,
    /// Per-document remembered state, most recently used first, capped at
    /// DOC_STATE_MAX documents.
    pub doc_state: Vec<DocState>,
    /// Recently opened documents, most recent first, capped at RECENT_MAX.
    pub recent_files: Vec<PathBuf>,
    /// Files frozen in the Files panel: kept there whatever falls off
    /// `recent_files`, until unfrozen, removed, or found to be gone.
    pub frozen_files: Vec<PathBuf>,
    /// Base URL of the Memos server, normalized. Empty when unconfigured.
    /// The access token is deliberately not here — see `crate::secrets`.
    pub memos_url: String,
    /// Memos being edited locally, most recently used first, capped at
    /// MEMO_DOC_MAX.
    pub memo_docs: Vec<MemoDoc>,
    /// Accept the Memos server's TLS certificate without verifying it.
    /// Off by default and deliberately not remembered per-memo: it is a
    /// property of the connection, and the UI keeps saying so while it is on.
    pub memos_insecure: bool,
}

/// Documents in the "Open Recent" menu.
pub const RECENT_MAX: usize = 10;

/// Documents to keep state for; the oldest entries fall off.
pub const DOC_STATE_MAX: usize = 200;

/// Memo ↔ cache-file links to remember; the oldest *pushed* entries fall
/// off (see `remember_memo`). An evicted entry only loses the memo's
/// identity, not its cached text, so a stale local file is left behind
/// rather than silently pushed somewhere — `memo_cache_stale` sweeps it up
/// later.
pub const MEMO_DOC_MAX: usize = 100;

/// How long an untouched checked-out memo stays in the cache directory.
/// Long enough that a document you are still living with is never swept,
/// short enough that a memo opened once and forgotten does not stay
/// forever.
pub const MEMO_CACHE_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A memo checked out into a local cache file. Bindle is path-based
/// throughout, so a fetched memo becomes an ordinary markdown file and this
/// is the only thing tying it back to the server.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct MemoDoc {
    /// The local cache file the memo is edited in.
    pub path: PathBuf,
    /// Resource name on the server, `memos/{uid}`.
    pub name: String,
    /// The memo's `updateTime` as of the last fetch or push, compared
    /// before pushing to notice edits made elsewhere meanwhile.
    pub update_time: String,
    /// Edited locally since that last push.
    pub unpushed: bool,
}

/// Insert `doc` at the front of the MRU list, replacing any entry for the
/// same file or the same memo, and trim back to `MEMO_DOC_MAX`.
///
/// Only entries with nothing unpushed are ever evicted. An unpushed entry
/// is the sole record that a local draft exists — drop it and the file
/// becomes indistinguishable from a stale leftover, which is exactly what
/// `memo_cache_stale` is allowed to delete. If every entry is a draft, the
/// list is left over its cap rather than discarding one.
pub fn remember_memo(docs: &mut Vec<MemoDoc>, doc: MemoDoc) {
    docs.retain(|d| d.path != doc.path && d.name != doc.name);
    docs.insert(0, doc);
    while docs.len() > MEMO_DOC_MAX {
        match docs.iter().rposition(|d| !d.unpushed) {
            Some(oldest_clean) => drop(docs.remove(oldest_clean)),
            None => break,
        }
    }
}

/// Whether a file in the memo cache directory may be deleted.
///
/// All three conditions are safety conditions, kept together so none can be
/// forgotten at a call site: it must be one of our own `.md` copies, it must
/// not be protected (currently open, or holding unpushed edits), and it must
/// have gone untouched for `MEMO_CACHE_MAX_AGE`. Everything that passes can
/// be fetched from the server again.
pub fn memo_cache_stale(path: &Path, age: Duration, protected: &[PathBuf]) -> bool {
    path.extension().is_some_and(|ext| ext == "md")
        && !protected.iter().any(|p| p == path)
        && age > MEMO_CACHE_MAX_AGE
}

/// Remembered state of one document. A struct (not a tuple) so fields can
/// be added without breaking the parse of existing settings files.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct DocState {
    pub path: PathBuf,
    /// Collapsed section keys.
    pub collapsed: Vec<String>,
    /// The preview occupied the main pane when the document was last shown.
    pub view: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    System,
    Light,
    Dark,
}

impl ColorScheme {
    pub fn from_str(s: &str) -> ColorScheme {
        match s {
            "light" => ColorScheme::Light,
            "dark" => ColorScheme::Dark,
            _ => ColorScheme::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ColorScheme::System => "system",
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        }
    }
}

/// Which palette paints the app. Independent of [`ColorScheme`]: each theme
/// has a light and a dark set, and the colour scheme chooses between them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// Bindle's sage/cream/cocoa palette — the default.
    Bindle,
    /// No overrides: whatever Adwaita and the user's accent colour give.
    System,
}

impl Theme {
    pub fn from_str(s: &str) -> Theme {
        match s {
            "system" => Theme::System,
            _ => Theme::Bindle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Bindle => "bindle",
            Theme::System => "system",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FontSize {
    Small,
    Normal,
    Large,
    XLarge,
}

impl FontSize {
    pub fn from_str(s: &str) -> FontSize {
        match s {
            "small" => FontSize::Small,
            "large" => FontSize::Large,
            "xlarge" => FontSize::XLarge,
            _ => FontSize::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FontSize::Small => "small",
            FontSize::Normal => "normal",
            FontSize::Large => "large",
            FontSize::XLarge => "xlarge",
        }
    }

    /// CSS size for the editor. All values land on fractional pixel sizes:
    /// integer-pixel sizes (multiples of 0.75pt) clip glyph top strokes with
    /// GTK's hinted rendering — see the load_css note in ui.rs.
    pub fn pt(self) -> &'static str {
        match self {
            FontSize::Small => "10.75pt",
            FontSize::Normal => "12.5pt",
            FontSize::Large => "14.5pt",
            FontSize::XLarge => "16.75pt",
        }
    }

    /// CSS size for the preview, whose base is smaller than the editor's
    /// (preview headings scale up from it via the h* tags). Same fractional
    /// rule as `pt`.
    pub fn preview_pt(self) -> &'static str {
        match self {
            FontSize::Small => "8.5pt",
            FontSize::Normal => "9.5pt",
            FontSize::Large => "11.5pt",
            FontSize::XLarge => "13.75pt",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemotePolicy {
    Ask,
    Always,
    Never,
}

impl RemotePolicy {
    pub fn from_str(s: &str) -> RemotePolicy {
        match s {
            "always" => RemotePolicy::Always,
            "never" => RemotePolicy::Never,
            _ => RemotePolicy::Ask,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RemotePolicy::Ask => "ask",
            RemotePolicy::Always => "always",
            RemotePolicy::Never => "never",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 1000,
            height: 700,
            maximized: false,
            hpaned: 300,
            vpaned: 320,
            files_paned: 200,
            last_file: None,
            remote_images: "ask".to_string(),
            color_scheme: "system".to_string(),
            theme: "bindle".to_string(),
            open_in_view: false,
            show_files: false,
            show_insert: true,
            show_preview: true,
            horizontal: false,
            open_last: true,
            multiple_instances: false,
            show_attachment_note: true,
            follow_cursor: true,
            keyword_highlight: true,
            line_numbers: false,
            preview_line_numbers: false,
            word_wrap: true,
            editor_font: "normal".to_string(),
            preview_font: "normal".to_string(),
            remember_folds: true,
            doc_state: Vec::new(),
            recent_files: Vec::new(),
            frozen_files: Vec::new(),
            memos_url: String::new(),
            memo_docs: Vec::new(),
            memos_insecure: false,
        }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("bindle").join("settings.json")
}

impl Settings {
    /// Read the settings file. A file that does not parse is *not* simply
    /// replaced by defaults at the next save: it holds the only record of
    /// unpushed memo drafts. Every field that still reads is kept, and the
    /// original is set aside as `settings.json.bad` for anything that doesn't.
    pub fn load() -> Settings {
        let path = path();
        let Ok(text) = fs::read_to_string(&path) else {
            return Settings::default();
        };
        if let Ok(settings) = serde_json::from_str(&text) {
            return settings;
        }
        let _ = fs::copy(&path, path.with_extension("json.bad"));
        serde_json::from_str(&text).map(salvage).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            // Rewritten on every toggle: a torn write would lose everything.
            let _ = crate::doc::write_atomic(&path, json.as_bytes());
        }
    }
}

/// The fields of a settings object that still deserialize, one at a time,
/// so one bad value costs that setting instead of all of them.
fn salvage(value: serde_json::Value) -> Settings {
    use serde_json::{Map, Value};
    let Value::Object(fields) = value else {
        return Settings::default();
    };
    let mut good = Map::new();
    for (key, field) in fields {
        good.insert(key.clone(), field);
        if serde_json::from_value::<Settings>(Value::Object(good.clone())).is_err() {
            good.remove(&key);
        }
    }
    serde_json::from_value(Value::Object(good)).unwrap_or_default()
}

/// The checked-out memos currently recorded in the settings file, for
/// `keep_foreign_drafts` on every save. Reads only that field and, unlike
/// `Settings::load`, has no side effects: a file that does not parse yields
/// nothing here rather than another `settings.json.bad` copy.
pub fn memo_docs_on_disk() -> Vec<MemoDoc> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Drafts {
        memo_docs: Vec<MemoDoc>,
    }
    fs::read_to_string(path())
        .ok()
        .and_then(|text| serde_json::from_str::<Drafts>(&text).ok())
        .map(|d| d.memo_docs)
        .unwrap_or_default()
}

/// Carry over unpushed drafts that another running instance recorded in the
/// settings file. Each instance writes its whole settings on every change,
/// so without this the last writer's memo list silently drops the other's
/// drafts — and a draft without its entry is fair game for the cache sweep.
/// An entry sharing a path or memo with one of ours is ours to decide.
pub fn keep_foreign_drafts(ours: &mut Vec<MemoDoc>, on_disk: &[MemoDoc]) {
    for doc in on_disk.iter().filter(|d| d.unpushed) {
        if !ours.iter().any(|o| o.path == doc.path || o.name == doc.name) {
            ours.push(doc.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, name: &str, unpushed: bool) -> MemoDoc {
        MemoDoc {
            path: PathBuf::from(path),
            name: name.to_string(),
            update_time: "2026-08-14T10:00:00Z".to_string(),
            unpushed,
        }
    }

    #[test]
    fn remembering_moves_an_entry_to_the_front() {
        let mut docs = vec![doc("/a.md", "memos/a", false), doc("/b.md", "memos/b", false)];
        remember_memo(&mut docs, doc("/b.md", "memos/b", true));
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].name, "memos/b");
        assert!(docs[0].unpushed);
    }

    #[test]
    fn remembering_replaces_a_renamed_file_for_the_same_memo() {
        // The slug follows the memo's first line, so the same memo must not
        // end up owning two cache files.
        let mut docs = vec![doc("/old-slug-a.md", "memos/a", false)];
        remember_memo(&mut docs, doc("/new-slug-a.md", "memos/a", false));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, PathBuf::from("/new-slug-a.md"));
    }

    #[test]
    fn eviction_stops_at_the_cap() {
        let mut docs = Vec::new();
        for i in 0..MEMO_DOC_MAX + 20 {
            remember_memo(&mut docs, doc(&format!("/{i}.md"), &format!("memos/{i}"), false));
        }
        assert_eq!(docs.len(), MEMO_DOC_MAX);
        // Most recent first, oldest gone.
        assert_eq!(docs[0].name, format!("memos/{}", MEMO_DOC_MAX + 19));
    }

    #[test]
    fn eviction_never_discards_an_unpushed_draft() {
        let mut docs = vec![doc("/draft.md", "memos/draft", true)];
        for i in 0..MEMO_DOC_MAX + 50 {
            remember_memo(&mut docs, doc(&format!("/{i}.md"), &format!("memos/{i}"), false));
        }
        assert_eq!(docs.len(), MEMO_DOC_MAX);
        // The draft is the oldest entry by far, yet it survived: losing it
        // would orphan its file and expose the draft to the cache sweep.
        assert!(docs.iter().any(|d| d.name == "memos/draft"));
    }

    #[test]
    fn a_list_of_only_drafts_is_never_trimmed() {
        let mut docs = Vec::new();
        for i in 0..MEMO_DOC_MAX + 5 {
            remember_memo(&mut docs, doc(&format!("/{i}.md"), &format!("memos/{i}"), true));
        }
        assert_eq!(docs.len(), MEMO_DOC_MAX + 5);
    }

    #[test]
    fn sweeps_only_old_unprotected_markdown() {
        let old = MEMO_CACHE_MAX_AGE + Duration::from_secs(1);
        let fresh = Duration::from_secs(60);
        let none: [PathBuf; 0] = [];

        assert!(memo_cache_stale(Path::new("/c/gone-a1.md"), old, &none));
        // Recently touched: someone may still be working on it.
        assert!(!memo_cache_stale(Path::new("/c/gone-a1.md"), fresh, &none));
        // Not one of ours — never delete a file we did not write.
        assert!(!memo_cache_stale(Path::new("/c/notes.txt"), old, &none));
        assert!(!memo_cache_stale(Path::new("/c/no-extension"), old, &none));
    }

    #[test]
    fn one_bad_field_does_not_cost_the_rest() {
        let value = serde_json::json!({
            "width": "not a number",
            "open_last": false,
            "memo_docs": [{ "path": "/c/a.md", "name": "memos/a", "unpushed": true }],
        });
        let settings = salvage(value);
        assert_eq!(settings.width, Settings::default().width);
        assert!(!settings.open_last);
        assert_eq!(settings.memo_docs.len(), 1);
        assert!(settings.memo_docs[0].unpushed);
    }

    #[test]
    fn another_instances_drafts_survive_our_save() {
        let mut ours = vec![doc("/a.md", "memos/a", false)];
        let disk = [
            doc("/a.md", "memos/a", true),      // ours: we decide
            doc("/b.md", "memos/b", true),      // theirs, unpushed: keep
            doc("/c.md", "memos/c", false),     // theirs, pushed: re-fetchable
        ];
        keep_foreign_drafts(&mut ours, &disk);
        assert_eq!(ours.len(), 2);
        assert!(!ours[0].unpushed);
        assert_eq!(ours[1].name, "memos/b");
    }

    #[test]
    fn protected_files_survive_at_any_age() {
        let ancient = MEMO_CACHE_MAX_AGE * 100;
        let protected = [PathBuf::from("/c/draft-a1.md")];
        assert!(!memo_cache_stale(Path::new("/c/draft-a1.md"), ancient, &protected));
        // A different file in the same directory is still fair game.
        assert!(memo_cache_stale(Path::new("/c/other-a2.md"), ancient, &protected));
    }
}
