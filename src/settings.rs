use std::fs;
use std::path::PathBuf;

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
    #[serde(with = "path_json::opt")]
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
    /// Shrink the preview's font (and images) so a full-width reading line
    /// fits the panel — an overview of the whole document's look.
    pub preview_fit: bool,
    /// Remember per-document state (collapsed sections, view mode) across
    /// sessions. JSON key kept from when it covered folds only.
    pub remember_folds: bool,
    /// Per-document remembered state, most recently used first, capped at
    /// DOC_STATE_MAX documents.
    pub doc_state: Vec<DocState>,
    /// Recently opened documents, most recent first, capped at RECENT_MAX.
    #[serde(with = "path_json::list")]
    pub recent_files: Vec<PathBuf>,
    /// Files frozen in the Files panel: kept there whatever falls off
    /// `recent_files`, until unfrozen, removed, or found to be gone.
    #[serde(with = "path_json::list")]
    pub frozen_files: Vec<PathBuf>,
}

/// Documents in the "Open Recent" menu.
pub const RECENT_MAX: usize = 10;

/// Documents to keep state for; the oldest entries fall off.
pub const DOC_STATE_MAX: usize = 200;

/// Remembered state of one document. A struct (not a tuple) so fields can
/// be added without breaking the parse of existing settings files.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct DocState {
    #[serde(with = "path_json")]
    pub path: PathBuf,
    /// Collapsed section keys.
    pub collapsed: Vec<String>,
    /// The preview occupied the main pane when the document was last shown.
    pub view: bool,
    /// Where the document was left: the editor cursor, and the first visible
    /// line of the editor and of the preview, all as source char offsets —
    /// text positions rather than pixels, so they survive a different
    /// window width or font size.
    pub cursor: i32,
    pub editor_top: i32,
    pub preview_top: i32,
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

/// Smallest preview font "Fit preview to width" shrinks to; below this the
/// overview stops being legible as text at all.
pub const FIT_MIN_PT: f64 = 5.0;

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
    pub fn preview_pt(self) -> f64 {
        match self {
            FontSize::Small => 8.5,
            FontSize::Normal => 9.5,
            FontSize::Large => 11.5,
            FontSize::XLarge => 13.75,
        }
    }

    /// The preview size shrunk by `scale` for "Fit preview to width": never
    /// below `FIT_MIN_PT`, never above the unscaled size, and on a quarter
    /// point that is not a whole pixel (the clipping rule above).
    pub fn fitted_preview_pt(self, scale: f64) -> f64 {
        let base = self.preview_pt();
        let mut pt = ((base * scale.min(1.0)) * 4.0).round() / 4.0;
        pt = pt.clamp(FIT_MIN_PT.min(base), base);
        // 0.75pt = 1px: nudge whole-pixel sizes off the grid.
        if (pt / 0.75).fract() == 0.0 {
            pt += 0.25;
        }
        pt
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
            preview_fit: false,
            remember_folds: true,
            doc_state: Vec::new(),
            recent_files: Vec::new(),
            frozen_files: Vec::new(),
        }
    }
}

fn path() -> PathBuf {
    glib::user_config_dir().join("bindle").join("settings.json")
}

impl Settings {
    /// Read the settings file. A file that does not parse is *not* simply
    /// replaced by defaults at the next save: every field that still reads
    /// is kept, and the original is set aside as `settings.json.bad` for
    /// anything that doesn't.
    pub fn load() -> Settings {
        let path = path();
        let bad = path.with_extension("json.bad");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                // Unreadable but present: the next save replaces it.
                if err.kind() != std::io::ErrorKind::NotFound {
                    let _ = fs::copy(&path, &bad);
                }
                return Settings::default();
            }
        };
        if let Ok(settings) = serde_json::from_slice(&bytes) {
            return settings;
        }
        let _ = fs::copy(&path, &bad);
        // Lossy only here: a stray non-UTF-8 byte costs the value it sits in.
        serde_json::from_str(&String::from_utf8_lossy(&bytes)).map(salvage).unwrap_or_default()
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

/// Paths as JSON strings, or as byte arrays when they are not UTF-8. A Linux
/// file name need not be, and serde's own `PathBuf` impl refuses those —
/// which failed the whole settings save for as long as one such file stayed
/// in the recent list.
mod path_json {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Bytes(Vec<u8>),
    }

    impl From<&Path> for Repr {
        fn from(path: &Path) -> Repr {
            match path.to_str() {
                Some(s) => Repr::Text(s.to_string()),
                None => Repr::Bytes(path.as_os_str().as_bytes().to_vec()),
            }
        }
    }

    impl From<Repr> for PathBuf {
        fn from(repr: Repr) -> PathBuf {
            match repr {
                Repr::Text(s) => PathBuf::from(s),
                Repr::Bytes(b) => PathBuf::from(OsString::from_vec(b)),
            }
        }
    }

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        Repr::from(path).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Repr::deserialize(d).map(PathBuf::from)
    }

    pub mod opt {
        use super::*;

        pub fn serialize<S: Serializer>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
            path.as_deref().map(Repr::from).serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
            Ok(Option::<Repr>::deserialize(d)?.map(PathBuf::from))
        }
    }

    pub mod list {
        use super::*;

        pub fn serialize<S: Serializer>(paths: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(paths.iter().map(|p| Repr::from(p.as_path())))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
            Ok(Vec::<Repr>::deserialize(d)?.into_iter().map(PathBuf::from).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fitted_preview_sizes_stay_in_range_and_off_whole_pixels() {
        for size in ["small", "normal", "large", "xlarge"] {
            let size = super::FontSize::from_str(size);
            for step in 0..=120 {
                let pt = size.fitted_preview_pt(step as f64 / 100.0);
                assert!(pt >= super::FIT_MIN_PT, "{pt}");
                assert!(pt <= size.preview_pt() + 0.25, "{pt}");
                assert_ne!((pt / 0.75).fract(), 0.0, "{pt} is a whole pixel");
            }
            assert_eq!(size.fitted_preview_pt(1.0), size.preview_pt());
        }
    }

    use super::*;

    #[test]
    fn one_bad_field_does_not_cost_the_rest() {
        let value = serde_json::json!({
            "width": "not a number",
            "open_last": false,
            "recent_files": ["/c/a.md"],
        });
        let settings = salvage(value);
        assert_eq!(settings.width, Settings::default().width);
        assert!(!settings.open_last);
        assert_eq!(settings.recent_files, [PathBuf::from("/c/a.md")]);
    }

    #[test]
    fn non_utf8_paths_save_and_load_back() {
        use std::os::unix::ffi::OsStringExt;
        let odd = PathBuf::from(std::ffi::OsString::from_vec(b"/c/bl\xE5.md".to_vec()));
        let plain = PathBuf::from("/c/a.md");
        let mut settings = Settings::default();
        settings.last_file = Some(odd.clone());
        settings.recent_files = vec![odd.clone(), plain.clone()];
        settings.doc_state = vec![DocState { path: odd.clone(), ..Default::default() }];
        let json = serde_json::to_string(&settings).expect("serializes");
        // UTF-8 paths keep their old, readable string form.
        assert!(json.contains("\"/c/a.md\""));
        let back: Settings = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.last_file, Some(odd.clone()));
        assert_eq!(back.recent_files, [odd.clone(), plain]);
        assert_eq!(back.doc_state[0].path, odd);
    }

    #[test]
    fn settings_from_a_memos_era_build_still_load() {
        // The Memos keys are gone from `Settings`; an old file still parses
        // instead of falling back to salvage.
        let text = r#"{ "open_last": false, "memos_url": "https://m.example", "memos_insecure": true,
                        "memo_docs": [{ "path": "/c/a.md", "name": "memos/a", "unpushed": true }] }"#;
        let settings: Settings = serde_json::from_str(text).expect("parses");
        assert!(!settings.open_last);
    }
}
