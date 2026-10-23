use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::bundle;
use crate::doc::{self, DocKind};
use crate::editor;
use crate::export;
use crate::highlight;
use crate::memos;
use crate::odf;
use crate::preview;
use crate::secrets;
use crate::settings::{
    self, ColorScheme, DocState, FontSize, MemoDoc, RemotePolicy, Settings, Theme, DOC_STATE_MAX,
    RECENT_MAX,
};
use crate::snippets::{self, Snippet};
use crate::theme;

thread_local! {
    static STATE: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Horizontal padding on each side of the line numbers in the gutter.
const GUTTER_PAD: i32 = 8;

/// Text width "Fit preview to width" fits into the panel, in ems of the
/// chosen preview font — roughly a full reading line (~80 characters).
const FIT_EMS: f64 = 40.0;

/// Above this, keyword highlighting switches itself off. Tagging is
/// incremental, but the scanners still read the whole document on every
/// debounce and nothing here can be done per-line — they take the whole text.
/// `cargo run --release --example hlbench 3000` measures that floor at ~17 ms
/// per megabyte of markdown (the slowest scanner), so this cap is about
/// 35 ms of main-loop work per debounce in the worst case.
const HIGHLIGHT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Which pass of an open this is — a file on an unmounted gvfs share needs
/// two, one to discover it is missing and one after the share is up.
#[derive(Clone, Copy, PartialEq)]
enum OpenTry {
    First,
    AfterMount,
}

/// Widgets and paging state of the Memos browse dialog, held in one `Rc` so
/// the search entry, the "Load more" button and the row handler all drive
/// the same list. `memos` is indexed by `ListBoxRow::index()`, which is why
/// "Load more" sits outside the list box.
struct Browse {
    dialog: adw::Dialog,
    list: gtk4::ListBox,
    /// Shown instead of the list while loading, empty or failed.
    status: gtk4::Label,
    more: gtk4::Button,
    spinner: gtk4::Spinner,
    memos: RefCell<Vec<memos::Memo>>,
    /// Token for the next page; empty once the list is exhausted.
    next_token: RefCell<String>,
    query: RefCell<String>,
    loading: Cell<bool>,
}

/// Where the current document lives.
#[derive(Clone)]
enum DocSource {
    /// Unsaved new document.
    None,
    /// A plain markdown file.
    Plain(PathBuf),
    /// A .textbundle directory; `text_name` is the markdown file inside it.
    Bundle { dir: PathBuf, text_name: String },
    /// A .textpack zip archive; assets are read from it lazily.
    Pack { path: PathBuf, info: bundle::PackInfo },
    /// An imported .odt. Never written back (see `odf`): Save goes to Save
    /// As. Its pictures are read lazily out of the package, like a pack's.
    Odt { path: PathBuf },
}

impl DocSource {
    /// The path shown in the title and remembered as the last file.
    fn container(&self) -> Option<&Path> {
        match self {
            DocSource::None => None,
            DocSource::Plain(path) => Some(path),
            DocSource::Bundle { dir, .. } => Some(dir),
            DocSource::Pack { path, .. } => Some(path),
            DocSource::Odt { path } => Some(path),
        }
    }

    /// The file whose contents are the document, for noticing when another
    /// program changes it: the markdown inside a bundle (its directory's
    /// mtime says nothing about an in-place rewrite), otherwise the
    /// container itself.
    fn text_file(&self) -> Option<PathBuf> {
        match self {
            DocSource::Bundle { dir, text_name } => Some(dir.join(text_name)),
            other => other.container().map(Path::to_path_buf),
        }
    }

    /// The zip archive local references resolve into, when the document
    /// lives in one.
    fn archive(&self) -> Option<(PathBuf, bundle::PackInfo)> {
        match self {
            DocSource::Pack { path, info } => Some((path.clone(), info.clone())),
            DocSource::Odt { path } => Some((
                path.clone(),
                bundle::PackInfo {
                    prefix: String::new(),
                    text_name: "content.xml".into(),
                    entries: Vec::new(),
                },
            )),
            _ => None,
        }
    }

    /// Directory against which relative image paths resolve.
    fn preview_base(&self) -> Option<PathBuf> {
        match self {
            DocSource::Plain(path) => path.parent().map(Path::to_path_buf),
            DocSource::Bundle { dir, .. } => Some(dir.clone()),
            _ => None,
        }
    }
}

pub struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    buffer: gtk4::TextBuffer,
    editor: gtk4::TextView,
    preview: gtk4::TextView,
    editor_scroll: gtk4::ScrolledWindow,
    preview_scroll: gtk4::ScrolledWindow,
    /// Recent documents, one click to open: the sidebar's top slot, so its
    /// rows stay put when a non-markdown file hides the panels below.
    files_panel: gtk4::Box,
    /// Rows mirror `files_order` by index.
    files_list: gtk4::ListBox,
    /// The Files panel's own order: `recent_files` as it stood at launch
    /// (newest first), then fixed for the session — a row that jumped to
    /// the top when clicked was a moving target. Files opened later join at
    /// the bottom; removed ones drop out in place. Frozen files stay even
    /// after they fall off the recent list.
    files_order: RefCell<Vec<PathBuf>>,
    /// Files frozen in the Files panel (the pin toggle), persisted.
    frozen_files: RefCell<Vec<PathBuf>>,
    top_panel: gtk4::Box,
    bottom_panel: gtk4::Box,
    bottom_label: gtk4::Label,
    hpaned: gtk4::Paned,
    /// The whole sidebar: Files above `lower` (Insert over Preview). Nested
    /// rather than one three-way split because GtkPaned holds two children;
    /// a hidden child gives its sibling all the room, so any subset works.
    sidebar: gtk4::Paned,
    /// Insert over Preview; its position is persisted as `Settings::vpaned`.
    lower: gtk4::Paned,
    insert_popover: gtk4::Popover,
    search: gtk4::SearchEntry,
    search_bar: gtk4::SearchBar,
    find_entry: gtk4::SearchEntry,
    /// "current/total" match counter in the find bar.
    find_count: gtk4::Label,
    /// Header collapse/expand-all button; icon mirrors the fold state.
    fold_btn: gtk4::Button,
    remote_banner: adw::Banner,
    /// "Changed on disk" — another program wrote the open file.
    disk_banner: adw::Banner,
    source: RefCell<DocSource>,
    /// Files added to a .textpack document, written into the zip on save.
    staged_assets: RefCell<HashMap<String, bundle::AssetSource>>,
    /// Pack asset bytes already read out of the zip this document; avoids
    /// re-opening and re-inflating the archive on every debounced render.
    pack_bytes_cache: RefCell<HashMap<String, Vec<u8>>>,
    /// True while an unsaved-changes dialog is up (blocks external opens).
    guard_active: Cell<bool>,
    modified: Cell<bool>,
    loading: Cell<bool>,
    preview_pending: RefCell<Option<glib::SourceId>>,
    /// Maps preview click positions back to markdown source positions.
    preview_map: RefCell<preview::SourceMap>,
    /// Clickable link ranges of the current preview.
    preview_links: RefCell<Vec<preview::Link>>,
    preview_images: RefCell<Vec<preview::Image>>,
    /// Task list checkboxes of the current preview.
    preview_checkboxes: RefCell<Vec<preview::Checkbox>>,
    /// Collapsible heading sections of the current preview.
    preview_sections: RefCell<Vec<preview::Section>>,
    /// Table lines of the current preview, for whole-row line picks.
    preview_table_rows: RefCell<Vec<preview::TableRow>>,
    /// Width tables were last fitted to (see `preview_table_cols`).
    preview_table_cols: Cell<Option<usize>>,
    /// Source char range selected from the line-number gutter; mirrored in
    /// the preview by the `line-pick` tag until the selection changes.
    line_pick: Cell<Option<(i32, i32)>>,
    /// Keys of collapsed sections (seeded from `fold_state` on load).
    collapsed: RefCell<HashSet<String>>,
    /// Per-document remembered state (folds, view mode), MRU-ordered,
    /// persisted in settings.
    doc_state: RefCell<Vec<DocState>>,
    /// Recently opened documents, most recent first, persisted in settings.
    recent_files: RefCell<Vec<PathBuf>>,
    /// Generation counter for async opens; a completion whose sequence no
    /// longer matches is stale (a newer open started) and gets dropped.
    open_seq: Cell<u64>,
    /// "Open Recent" submenu, rebuilt from recent_files on every open.
    recent_menu: gio::Menu,
    /// Menu toggle: remember per-document state across sessions.
    remember_folds: Cell<bool>,
    /// Set while a preview click moves the editor cursor, so the
    /// cursor-follow handler doesn't scroll the preview back.
    syncing_cursor: Cell<bool>,
    remote_policy: Cell<RemotePolicy>,
    /// Per-document consent to load remote images (reset on new/open).
    doc_allows_remote: Cell<bool>,
    color_scheme: Cell<ColorScheme>,
    /// Which palette paints the app: Bindle's own, or stock Adwaita.
    theme: Cell<Theme>,
    /// Adwaita named-colour overrides for `theme`; empty for Theme::System.
    theme_provider: gtk4::CssProvider,
    /// View mode: preview is the main pane, editor sits in the sidebar.
    view_mode: Cell<bool>,
    open_in_view: Cell<bool>,
    open_last: Cell<bool>,
    multiple_instances: Cell<bool>,
    show_attachment_note: Cell<bool>,
    follow_cursor: Cell<bool>,
    keyword_highlight: Cell<bool>,
    /// Per-line highlight signatures as of the last `apply_highlight`, so the
    /// next one can re-tag only the lines whose highlighting actually moved.
    /// Empty means "no usable baseline" and forces a full pass. Shared with
    /// the buffer's edit signals (`highlight::track_edits`), which keep it
    /// lined up with the buffer's lines between passes.
    hl_sigs: Rc<RefCell<Vec<u64>>>,
    /// Line-number gutter widget, attached to the editor's left text window
    /// while `line_numbers` is on.
    gutter: gtk4::DrawingArea,
    line_numbers: Cell<bool>,
    /// The preview's gutter: source line numbers beside the rendered lines,
    /// attached to the preview's left text window while
    /// `preview_line_numbers` is on.
    preview_gutter: gtk4::DrawingArea,
    preview_line_numbers: Cell<bool>,
    /// Editor word wrap; overridden off for kinds that need exact line
    /// widths (`DocKind::wrap_lines`), never written back.
    word_wrap: Cell<bool>,
    editor_font: Cell<FontSize>,
    preview_font: Cell<FontSize>,
    /// "Fit preview to width": the preview font (and images) shrink with the
    /// panel so a reading line keeps its full-size words — see `fit_scale`.
    preview_fit: Cell<bool>,
    /// The scale the preview font was last applied at.
    preview_scale: Cell<f64>,
    /// Pending debounced re-fit after a width change, if any.
    fit_pending: Cell<Option<glib::SourceId>>,
    /// Runtime override of the editor/preview font-size CSS from load_css.
    font_provider: gtk4::CssProvider,
    show_files: Cell<bool>,
    show_insert: Cell<bool>,
    show_preview: Cell<bool>,
    /// Horizontal layout: main pane on top, insert + preview below.
    horizontal: Cell<bool>,
    /// Paned positions of the other layout, restored when toggling back.
    saved_panes: Cell<Option<(i32, i32, i32)>>,
    /// Base URL of the Memos server; empty until configured. The access
    /// token is deliberately not kept here — every request reads it from
    /// the keyring on its worker thread (see `secrets`).
    memos_url: RefCell<String>,
    /// Memos checked out into local cache files, MRU-ordered, persisted.
    memo_docs: RefCell<Vec<MemoDoc>>,
    /// Talk to the Memos server without verifying its certificate.
    memos_insecure: Cell<bool>,
    /// The current document is a checked-out memo edited since its last
    /// push. Seeded from the store on load, so it survives a restart.
    memo_unpushed: Cell<bool>,
    /// The document's file as last read or written by us (`text_file`).
    /// `None` for an unsaved document, or when the stat failed.
    disk_stamp: Cell<Option<doc::DiskStamp>>,
    /// A disk check is on a worker thread; on a hung mount further focus
    /// changes must not pile up more of them.
    disk_checking: Cell<bool>,
}

fn apply_color_scheme(scheme: ColorScheme) {
    adw::StyleManager::default().set_color_scheme(match scheme {
        ColorScheme::System => adw::ColorScheme::Default,
        ColorScheme::Light => adw::ColorScheme::ForceLight,
        ColorScheme::Dark => adw::ColorScheme::ForceDark,
    });
}

pub fn load_css() {
    // Fractional pt sizes on purpose: at integer-pixel sizes (12pt = 16px)
    // GTK's hinted glyph rendering clips the top strokes of T/F glyphs.
    // Env override kept for examples/uishot.rs rendering experiments.
    let editor_size = std::env::var("BINDLE_EDITOR_FONT").unwrap_or_else(|_| "12.5pt".into());
    let css = format!("
        textview.editor-view {{ font-size: {editor_size}; }}
        textview.editor-view, textview.editor-view text {{ background: transparent; }}
        textview.preview-view {{ font-size: 9.5pt; }}
        textview.preview-view, textview.preview-view text {{ background: transparent; }}
        textview.preview-view border {{ background: transparent; }}
        .sidebar-panels {{ background-color: var(--sidebar-bg-color, alpha(currentColor, 0.04)); }}
        .files-freeze {{ min-height: 0; min-width: 0; padding: 2px; }}
        .files-freeze:not(:checked) {{ opacity: 0; }}
        row:hover .files-freeze:not(:checked) {{ opacity: 0.55; }}
        .files-freeze:checked {{ background: none; }}
        .preview-card {{ background-color: var(--view-bg-color, #ffffff); border-radius: 8px; }}
    ");
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Present the (single) main window, optionally opening `file` in it.
pub fn present(app: &adw::Application, file: Option<PathBuf>) {
    let state = STATE.with(|s| s.borrow().clone());
    let state = match state {
        Some(state) => state,
        None => {
            let state = App::build(app);
            STATE.with(|s| *s.borrow_mut() = Some(state.clone()));
            if let Some(path) = &file {
                state.preset_view_mode(path);
            }
            state
        }
    };
    state.window.present();
    state.focus_main();
    // Files/CLI can open into a window holding unsaved work — ask first.
    // While a guard dialog is already up, don't stack a second one with its
    // own open continuation; the requests would clobber each other.
    if let Some(path) = file {
        if state.guard_active.get() {
            state.toast("Finish the unsaved changes dialog first");
        } else {
            let s = state.clone();
            state.guard_unsaved(Box::new(move || s.open_path(&path)));
        }
    }
}

impl App {
    fn build(app: &adw::Application) -> Rc<App> {
        let settings = Settings::load();

        // --- Editor (right column) ---
        let buffer = gtk4::TextBuffer::new(None);
        buffer.set_enable_undo(true);
        highlight::setup_tags(&buffer);
        let editor = gtk4::TextView::with_buffer(&buffer);
        editor.set_wrap_mode(gtk4::WrapMode::WordChar);
        editor.set_monospace(true);
        editor.add_css_class("editor-view");
        editor.set_left_margin(24);
        editor.set_right_margin(24);
        editor.set_top_margin(18);
        editor.set_bottom_margin(18);
        editor.set_pixels_above_lines(2);

        // Line-number gutter: numbers one paragraph per buffer line, aligned
        // via the editor's own coordinate mapping so wrap and scroll just work.
        let gutter = gtk4::DrawingArea::new();
        {
            let view = editor.clone();
            gutter.set_draw_func(move |area, cr, width, height| {
                let rect = view.visible_rect();
                let (mut iter, _) = view.line_at_y(rect.y());
                let color = area.color();
                // Lines holding the cursor or a selection get the full
                // foreground color, the rest stay dimmed.
                let buffer = view.buffer();
                let (hl_first, hl_last) = match buffer.selection_bounds() {
                    // A selection ending at a line start (a whole-line pick
                    // from the gutter) does not reach into that line.
                    Some((start, end)) if end.starts_line() && end.line() > start.line() => {
                        (start.line(), end.line() - 1)
                    }
                    Some((start, end)) => (start.line(), end.line()),
                    None => {
                        let line = buffer.iter_at_mark(&buffer.get_insert()).line();
                        (line, line)
                    }
                };
                let above = view.pixels_above_lines();
                loop {
                    let (y, _) = view.line_yrange(&iter);
                    let (_, wy) = view.buffer_to_window_coords(gtk4::TextWindowType::Left, 0, y);
                    if wy > height {
                        break;
                    }
                    let line = iter.line();
                    let alpha = if (hl_first..=hl_last).contains(&line) { 1.0 } else { 0.45 };
                    cr.set_source_rgba(
                        color.red() as f64,
                        color.green() as f64,
                        color.blue() as f64,
                        color.alpha() as f64 * alpha,
                    );
                    let layout = view.create_pango_layout(Some(&(line + 1).to_string()));
                    let (lw, _) = layout.pixel_size();
                    cr.move_to((width - GUTTER_PAD - lw) as f64, (wy + above) as f64);
                    pangocairo::functions::show_layout(cr, &layout);
                    // False also when it lands on an empty last line (a file
                    // ending in a newline) — that line still gets a number.
                    if !iter.forward_line() && iter.line() == line {
                        break;
                    }
                }
            });
        }

        let editor_scroll = gtk4::ScrolledWindow::builder()
            .child(&editor)
            .hexpand(true)
            .vexpand(true)
            .build();
        editor_scroll.set_size_request(320, -1);

        // --- Element list (left top) ---
        let elements = snippet_listbox();
        let elements_scroll = gtk4::ScrolledWindow::builder()
            .child(&elements)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        let (top_panel, _) = section("Insert", Some("Ctrl+Space"), &elements_scroll);

        // --- Files (sidebar top) ---
        let files_list = gtk4::ListBox::new();
        files_list.set_selection_mode(gtk4::SelectionMode::Single);
        files_list.add_css_class("navigation-sidebar");
        let files_scroll = gtk4::ScrolledWindow::builder()
            .child(&files_list)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        let (files_panel, _) = section("Files", None, &files_scroll);
        let files_empty = gtk4::Label::new(Some("No recent documents"));
        files_empty.add_css_class("dim-label");
        files_empty.set_margin_top(12);
        files_list.set_placeholder(Some(&files_empty));

        // --- Preview (left bottom) ---
        let preview_buffer = gtk4::TextBuffer::new(None);
        preview::setup_tags(&preview_buffer);
        // Find-highlight tags for editor and preview alike. GNOME yellow for
        // every hit, orange for the current one — added last so its
        // background wins where both tags overlap.
        for b in [&buffer, &preview_buffer] {
            let table = b.tag_table();
            table.add(&gtk4::TextTag::builder().name("find-match").background("#f6d32d4d").build());
            table.add(&gtk4::TextTag::builder().name("find-current").background("#ff78008c").build());
        }
        // Mirror of a line-number pick in the editor: translucent sage across
        // the full width of each line, so it reads on either palette and
        // search hits stay visible through it.
        preview_buffer.tag_table().add(
            &gtk4::TextTag::builder()
                .name("line-pick")
                .paragraph_background("#7fa07a59")
                .build(),
        );
        let preview = gtk4::TextView::with_buffer(&preview_buffer);
        preview.set_editable(false);
        preview.set_cursor_visible(false);
        preview.set_wrap_mode(gtk4::WrapMode::WordChar);
        preview.add_css_class("preview-view");
        preview.set_left_margin(12);
        preview.set_right_margin(12);
        preview.set_top_margin(10);
        preview.set_bottom_margin(10);
        let preview_scroll = gtk4::ScrolledWindow::builder()
            .child(&preview)
            .vexpand(true)
            .build();
        preview_scroll.add_css_class("preview-card");
        preview_scroll.set_margin_start(8);
        preview_scroll.set_margin_end(8);
        preview_scroll.set_margin_bottom(8);
        let (bottom_panel, bottom_label) = section("Preview", None, &preview_scroll);

        // --- Panes ---
        let lower = gtk4::Paned::new(gtk4::Orientation::Vertical);
        lower.set_start_child(Some(&top_panel));
        lower.set_end_child(Some(&bottom_panel));
        lower.set_shrink_start_child(false);
        lower.set_shrink_end_child(false);
        lower.set_position(settings.vpaned);
        top_panel.set_size_request(-1, 120);
        bottom_panel.set_size_request(-1, 120);
        files_panel.set_size_request(-1, 120);

        let sidebar = gtk4::Paned::new(gtk4::Orientation::Vertical);
        sidebar.set_start_child(Some(&files_panel));
        sidebar.set_end_child(Some(&lower));
        sidebar.set_shrink_start_child(false);
        sidebar.set_shrink_end_child(false);
        // A taller window grows the panels below, not the file list.
        sidebar.set_resize_start_child(false);
        sidebar.set_position(settings.files_paned);
        sidebar.add_css_class("sidebar-panels");

        let hpaned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        hpaned.set_start_child(Some(&sidebar));
        hpaned.set_end_child(Some(&editor_scroll));
        hpaned.set_shrink_start_child(false);
        hpaned.set_shrink_end_child(false);
        hpaned.set_resize_start_child(false);
        hpaned.set_position(settings.hpaned);
        sidebar.set_size_request(200, -1);

        // --- Header bar ---
        let header = adw::HeaderBar::new();
        let title = adw::WindowTitle::new("Untitled", "");
        header.set_title_widget(Some(&title));

        let new_btn = gtk4::Button::from_icon_name("document-new-symbolic");
        new_btn.set_tooltip_text(Some("New document (Ctrl+N)"));
        new_btn.set_action_name(Some("win.new"));
        let open_btn = gtk4::Button::from_icon_name("document-open-symbolic");
        open_btn.set_tooltip_text(Some("Open… (Ctrl+O)"));
        open_btn.set_action_name(Some("win.open"));
        header.pack_start(&new_btn);
        header.pack_start(&open_btn);

        let view_btn = gtk4::ToggleButton::new();
        view_btn.set_icon_name("object-flip-horizontal-symbolic");
        view_btn.set_tooltip_text(Some("Swap view/edit mode (Ctrl+E)"));
        view_btn.set_action_name(Some("win.view-mode"));
        header.pack_start(&view_btn);

        // Mirrors the preview's disclosure markers: ▾ while sections are
        // expanded (click collapses all), ▸ once everything is collapsed.
        let fold_btn = gtk4::Button::from_icon_name("pan-down-symbolic");
        fold_btn.set_tooltip_text(Some("Collapse or expand all sections"));
        fold_btn.set_action_name(Some("win.fold-all"));
        fold_btn.set_sensitive(false);
        header.pack_start(&fold_btn);

        // Primary menu: sectioned per the GNOME HIG — file operations,
        // view options, opening behavior, then About.
        let menu = gio::Menu::new();

        let file_section = gio::Menu::new();
        // Filled by update_recent_menu; items carry the path as action target.
        let recent_menu = gio::Menu::new();
        file_section.append_submenu(Some("Open _Recent"), &recent_menu);
        file_section.append(Some("Open _Bundle…"), Some("win.open-bundle"));
        file_section.append(Some("Save _As…"), Some("win.save-as"));
        let export_menu = gio::Menu::new();
        export_menu.append(Some("As Text_Pack…"), Some("win.export-pack"));
        export_menu.append(Some("As _HTML…"), Some("win.export-html"));
        export_menu.append(Some("As P_DF…"), Some("win.export-pdf"));
        export_menu.append(Some("As _ODT…"), Some("win.export-odt"));
        file_section.append_submenu(Some("_Export"), &export_menu);
        file_section.append(Some("_Print…"), Some("win.print"));
        menu.append_section(None, &file_section);

        let memos_section = gio::Menu::new();
        let memos_menu = gio::Menu::new();
        memos_menu.append(Some("_Send to Memos…"), Some("win.memos-send"));
        memos_menu.append(Some("_Browse Memos…"), Some("win.memos-browse"));
        memos_menu.append(Some("_Push to Memos"), Some("win.memos-push"));
        memos_menu.append(Some("Server Set_up…"), Some("win.memos-setup"));
        memos_section.append_submenu(Some("Me_mos"), &memos_menu);
        menu.append_section(None, &memos_section);

        let view_section = gio::Menu::new();
        let appearance_menu = gio::Menu::new();
        appearance_menu.append(Some("Follow system"), Some("win.color-scheme::system"));
        appearance_menu.append(Some("Light"), Some("win.color-scheme::light"));
        appearance_menu.append(Some("Dark"), Some("win.color-scheme::dark"));
        let palette_section = gio::Menu::new();
        palette_section.append(Some("Bindle colours"), Some("win.theme::bindle"));
        palette_section.append(Some("System colours"), Some("win.theme::system"));
        appearance_menu.append_section(Some("Palette"), &palette_section);
        view_section.append_submenu(Some("A_ppearance"), &appearance_menu);
        let panels_menu = gio::Menu::new();
        panels_menu.append(Some("Show _Files panel"), Some("win.show-files"));
        panels_menu.append(Some("Show _Insert panel"), Some("win.show-insert"));
        panels_menu.append(Some("Show _Preview panel"), Some("win.show-preview"));
        panels_menu.append(Some("_Horizontal layout"), Some("win.horizontal"));
        view_section.append_submenu(Some("Panel_s"), &panels_menu);
        let font_menu = gio::Menu::new();
        font_menu.append(Some("Small"), Some("win.editor-font::small"));
        font_menu.append(Some("Normal"), Some("win.editor-font::normal"));
        font_menu.append(Some("Large"), Some("win.editor-font::large"));
        font_menu.append(Some("Extra large"), Some("win.editor-font::xlarge"));
        view_section.append_submenu(Some("Editor _font size"), &font_menu);
        let preview_font_menu = gio::Menu::new();
        preview_font_menu.append(Some("Small"), Some("win.preview-font::small"));
        preview_font_menu.append(Some("Normal"), Some("win.preview-font::normal"));
        preview_font_menu.append(Some("Large"), Some("win.preview-font::large"));
        preview_font_menu.append(Some("Extra large"), Some("win.preview-font::xlarge"));
        view_section.append_submenu(Some("Preview f_ont size"), &preview_font_menu);
        view_section.append(Some("Fit preview to wi_dth"), Some("win.preview-fit"));
        view_section.append(Some("Line _numbers"), Some("win.line-numbers"));
        view_section.append(Some("Line numbers in p_review"), Some("win.preview-line-numbers"));
        view_section.append(Some("_Word wrap"), Some("win.word-wrap"));
        view_section.append(Some("Highlight _keywords"), Some("win.keyword-highlight"));
        view_section.append(Some("Preview follows _cursor"), Some("win.follow-cursor"));
        menu.append_section(None, &view_section);

        let behavior_section = gio::Menu::new();
        let remote_menu = gio::Menu::new();
        remote_menu.append(Some("Ask per document"), Some("win.remote-images::ask"));
        remote_menu.append(Some("Always load"), Some("win.remote-images::always"));
        remote_menu.append(Some("Never load"), Some("win.remote-images::never"));
        behavior_section.append_submenu(Some("_Web Images"), &remote_menu);
        behavior_section.append(Some("Open files in _view mode"), Some("win.open-in-view"));
        behavior_section.append(Some("Open _last file at launch"), Some("win.open-last"));
        behavior_section.append(Some("_Remember document state"), Some("win.remember-folds"));
        behavior_section.append(Some("Allow _multiple instances"), Some("win.multiple-instances"));
        menu.append_section(None, &behavior_section);

        let about_section = gio::Menu::new();
        about_section.append(Some("Abou_t Bindle"), Some("win.about"));
        menu.append_section(None, &about_section);
        let menu_btn = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main menu")
            .build();
        header.pack_end(&menu_btn);

        let save_btn = gtk4::Button::from_icon_name("document-save-symbolic");
        save_btn.set_tooltip_text(Some("Save (Ctrl+S)"));
        save_btn.set_action_name(Some("win.save"));
        header.pack_end(&save_btn);

        let insert_content = adw::ButtonContent::builder()
            .icon_name("list-add-symbolic")
            .label("Insert")
            .build();
        let insert_btn = gtk4::Button::builder()
            .child(&insert_content)
            .tooltip_text("Insert markdown element at cursor (Ctrl+Space)")
            .build();
        insert_btn.set_action_name(Some("win.insert"));
        header.pack_end(&insert_btn);

        // --- Insert popover (anchored at the text cursor) ---
        let insert_popover = gtk4::Popover::new();
        insert_popover.set_parent(&editor);
        insert_popover.set_position(gtk4::PositionType::Bottom);
        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Search elements…"));
        let popover_list = snippet_listbox();
        let popover_scroll = gtk4::ScrolledWindow::builder()
            .child(&popover_list)
            .min_content_height(280)
            .min_content_width(230)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        let popover_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        popover_box.append(&search);
        popover_box.append(&popover_scroll);
        insert_popover.set_child(Some(&popover_box));

        // --- Find bar (Ctrl+F) ---
        let find_entry = gtk4::SearchEntry::new();
        find_entry.set_placeholder_text(Some("Find in document…"));
        find_entry.set_hexpand(true);
        let prev_btn = gtk4::Button::from_icon_name("go-up-symbolic");
        prev_btn.set_tooltip_text(Some("Previous match (Shift+Enter)"));
        let next_btn = gtk4::Button::from_icon_name("go-down-symbolic");
        next_btn.set_tooltip_text(Some("Next match (Enter)"));
        let find_nav = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        find_nav.add_css_class("linked");
        find_nav.append(&prev_btn);
        find_nav.append(&next_btn);
        let find_count = gtk4::Label::new(None);
        find_count.add_css_class("dim-label");
        find_count.add_css_class("numeric");
        let find_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        find_box.append(&find_entry);
        find_box.append(&find_count);
        find_box.append(&find_nav);
        let search_bar = gtk4::SearchBar::new();
        search_bar.set_child(Some(&find_box));
        search_bar.connect_entry(&find_entry);
        search_bar.set_show_close_button(true);

        // --- Window ---
        let remote_banner = adw::Banner::new("This document references images on the web");
        remote_banner.set_button_label(Some("Load"));
        let disk_banner = adw::Banner::new("");
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&hpaned));
        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.add_top_bar(&search_bar);
        toolbar_view.add_top_bar(&remote_banner);
        toolbar_view.add_top_bar(&disk_banner);
        toolbar_view.set_content(Some(&toasts));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Bindle")
            .default_width(settings.width)
            .default_height(settings.height)
            .build();
        if settings.maximized {
            window.maximize();
        }
        window.set_content(Some(&toolbar_view));

        // Overrides the load_css editor font size; higher priority so the
        // menu choice wins over the baseline stylesheet.
        let font_provider = gtk4::CssProvider::new();
        // Redefines Adwaita's named colours for the Bindle palette. Sits at
        // APPLICATION priority, above the THEME-priority Adwaita stylesheet
        // whose definitions it replaces, and below font_provider so the two
        // never contend.
        let theme_provider = gtk4::CssProvider::new();
        if let Some(display) = gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(
                &display,
                &theme_provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            gtk4::style_context_add_provider_for_display(
                &display,
                &font_provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
            );
        }

        let state = Rc::new(App {
            window,
            toasts,
            title,
            buffer,
            editor,
            preview,
            editor_scroll,
            preview_scroll,
            files_panel,
            files_list,
            top_panel,
            bottom_panel,
            bottom_label,
            hpaned,
            sidebar,
            lower,
            insert_popover,
            search,
            search_bar,
            find_entry,
            find_count,
            fold_btn,
            remote_banner,
            disk_banner,
            source: RefCell::new(DocSource::None),
            staged_assets: RefCell::new(HashMap::new()),
            pack_bytes_cache: RefCell::new(HashMap::new()),
            guard_active: Cell::new(false),
            modified: Cell::new(false),
            loading: Cell::new(false),
            preview_pending: RefCell::new(None),
            preview_map: RefCell::new(preview::SourceMap::default()),
            preview_links: RefCell::new(Vec::new()),
            preview_images: RefCell::new(Vec::new()),
            preview_checkboxes: RefCell::new(Vec::new()),
            preview_sections: RefCell::new(Vec::new()),
            preview_table_rows: RefCell::new(Vec::new()),
            preview_table_cols: Cell::new(None),
            line_pick: Cell::new(None),
            collapsed: RefCell::new(HashSet::new()),
            doc_state: RefCell::new(settings.doc_state.clone()),
            recent_files: RefCell::new(settings.recent_files.clone()),
            files_order: RefCell::new(settings.recent_files.clone()),
            frozen_files: RefCell::new(settings.frozen_files.clone()),
            open_seq: Cell::new(0),
            recent_menu,
            remember_folds: Cell::new(settings.remember_folds),
            syncing_cursor: Cell::new(false),
            remote_policy: Cell::new(RemotePolicy::from_str(&settings.remote_images)),
            doc_allows_remote: Cell::new(false),
            color_scheme: Cell::new(ColorScheme::from_str(&settings.color_scheme)),
            theme: Cell::new(Theme::from_str(&settings.theme)),
            theme_provider,
            view_mode: Cell::new(false),
            open_in_view: Cell::new(settings.open_in_view),
            open_last: Cell::new(settings.open_last),
            multiple_instances: Cell::new(settings.multiple_instances),
            show_attachment_note: Cell::new(settings.show_attachment_note),
            follow_cursor: Cell::new(settings.follow_cursor),
            keyword_highlight: Cell::new(settings.keyword_highlight),
            hl_sigs: Rc::new(RefCell::new(Vec::new())),
            gutter,
            line_numbers: Cell::new(settings.line_numbers),
            preview_gutter: gtk4::DrawingArea::new(),
            preview_line_numbers: Cell::new(settings.preview_line_numbers),
            word_wrap: Cell::new(settings.word_wrap),
            editor_font: Cell::new(FontSize::from_str(&settings.editor_font)),
            preview_font: Cell::new(FontSize::from_str(&settings.preview_font)),
            preview_fit: Cell::new(settings.preview_fit),
            preview_scale: Cell::new(1.0),
            fit_pending: Cell::new(None),
            font_provider,
            show_files: Cell::new(settings.show_files),
            show_insert: Cell::new(settings.show_insert),
            show_preview: Cell::new(settings.show_preview),
            horizontal: Cell::new(false),
            saved_panes: Cell::new(None),
            memos_url: RefCell::new(settings.memos_url.clone()),
            memo_docs: RefCell::new(settings.memo_docs.clone()),
            memos_insecure: Cell::new(settings.memos_insecure),
            memo_unpushed: Cell::new(false),
            disk_stamp: Cell::new(None),
            disk_checking: Cell::new(false),
        });
        state.apply_panels();
        state.apply_fonts();
        state.apply_line_numbers(settings.line_numbers);
        state.apply_preview_line_numbers(settings.preview_line_numbers);
        state.apply_wrap();
        if settings.horizontal {
            state.apply_horizontal(true);
            // The saved positions are horizontal-layout values; reapply them
            // after the orientation switch.
            state.hpaned.set_position(settings.hpaned);
            state.sidebar.set_position(settings.files_paned);
            state.lower.set_position(settings.vpaned);
        }
        apply_color_scheme(state.color_scheme.get());
        state.apply_theme();
        // Each theme has a light and a dark set, so the palette has to be
        // rebuilt whenever the effective scheme flips — including when the
        // *system* flips it under ColorScheme::System, which no action of
        // ours would otherwise notice.
        let s = state.clone();
        adw::StyleManager::default().connect_dark_notify(move |_| s.apply_theme());

        let s = state.clone();
        prev_btn.connect_clicked(move |_| s.find_next(true));
        let s = state.clone();
        next_btn.connect_clicked(move |_| s.find_next(false));

        state.wire_signals(&elements, &popover_list);
        state.register_actions();
        state.update_recent_menu();
        // Nothing is loaded yet, so "Push to Memos" starts insensitive.
        state.update_memos_actions();

        // Restore the last opened file if enabled. No is_file() probe: it
        // could hang on an unreachable mount; open_path fails with a toast
        // off the main thread instead.
        let last = settings.last_file.filter(|_| settings.open_last);
        match last {
            Some(path) => {
                state.preset_view_mode(&path);
                state.open_path(&path);
            }
            None => state.update_title(),
        }

        state
    }

    fn wire_signals(self: &Rc<Self>, elements: &gtk4::ListBox, popover_list: &gtk4::ListBox) {
        highlight::track_edits(&self.buffer, self.hl_sigs.clone());

        // Editor changes: track modified state and refresh the preview.
        // The gutter updates first — also for programmatic loads.
        let state = self.clone();
        self.buffer.connect_changed(move |_| {
            state.update_gutter();
            if state.loading.get() {
                return;
            }
            let first_change = !state.modified.replace(true);
            // A checked-out memo now differs from the server copy. Persist
            // that immediately rather than at the next settings write:
            // opening another document replaces `source`, and this is the
            // last moment the flag can be attributed to the right memo.
            let newly_unpushed =
                state.memo_doc().is_some() && !state.memo_unpushed.replace(true);
            if first_change || newly_unpushed {
                state.update_title();
            }
            if newly_unpushed {
                state.persist_settings();
            }
            state.schedule_preview();
        });

        // Cursor / selection moves shift the gutter's highlighted line.
        let state = self.clone();
        self.buffer.connect_mark_set(move |_, _, mark| {
            if state.line_numbers.get()
                && matches!(mark.name().as_deref(), Some("insert" | "selection_bound"))
            {
                state.gutter.queue_draw();
            }
            // Any selection change other than the pick itself drops the
            // preview's mirror of it.
            if let Some((start, end)) = state.line_pick.get() {
                let same = state
                    .buffer
                    .selection_bounds()
                    .is_some_and(|(s, e)| (s.offset(), e.offset()) == (start, end));
                if !same {
                    state.line_pick.set(None);
                    state.apply_line_pick();
                }
            }
        });

        // Scrolling and size changes move the visible lines under the gutter.
        let adj = self.editor_scroll.vadjustment();
        let state = self.clone();
        adj.connect_value_changed(move |_| {
            if state.line_numbers.get() {
                state.gutter.queue_draw();
            }
        });
        let state = self.clone();
        adj.connect_changed(move |_| {
            if state.line_numbers.get() {
                state.gutter.queue_draw();
            }
        });

        // Double-click a line number selects that line; Shift+click extends
        // the current selection to cover it as well.
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let state = self.clone();
        click.connect_pressed(move |gesture, n_press, _, y| {
            let shift = gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            if shift || n_press == 2 {
                state.select_gutter_line(y, shift);
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            }
        });
        self.gutter.add_controller(click);

        // The preview gutter: drawn from the source map, so it lives here
        // rather than in `build` with the editor's; same picks as the editor
        // gutter, on the source lines a preview line came from.
        let state = self.clone();
        self.preview_gutter
            .set_draw_func(move |area, cr, width, height| state.draw_preview_gutter(area, cr, width, height));
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let state = self.clone();
        click.connect_pressed(move |gesture, n_press, _, y| {
            let shift = gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            if !(shift || n_press == 2) {
                return;
            }
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let (_, by) = state.preview.window_to_buffer_coords(gtk4::TextWindowType::Left, 0, y as i32);
            let (mut start, _) = state.preview.line_at_y(by);
            start.set_line_offset(0);
            let mut end = start;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            if let Some(lines) = state.preview_source_lines(start.offset(), end.offset()) {
                state.pick_from_preview(lines, shift);
            }
        });
        self.preview_gutter.add_controller(click);
        // Resizing the window, the panes or the gutter changes the text
        // area's width, which tables are wrapped to.
        if let Some(adj) = self.preview.hadjustment() {
            let state = self.clone();
            adj.connect_notify_local(Some("page-size"), move |_, _| {
                state.schedule_fit();
                state.refit_tables();
            });
        }
        if let Some(adj) = self.preview.vadjustment() {
            for signal in ["value-changed", "changed"] {
                let gutter = self.preview_gutter.clone();
                adj.connect_local(signal, false, move |_| {
                    gutter.queue_draw();
                    None
                });
            }
        }

        // Sidebar element list inserts on click.
        let state = self.clone();
        elements.connect_row_activated(move |_, row| {
            let snippet = &snippets::all()[row.index() as usize];
            state.insert_snippet(snippet);
        });

        // Popover list inserts and closes the popover.
        let state = self.clone();
        popover_list.connect_row_activated(move |_, row| {
            state.insert_popover.popdown();
            let snippet = &snippets::all()[row.index() as usize];
            state.insert_snippet(snippet);
        });

        // Popover search: filter the list, Enter picks the first match.
        let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let q = query.clone();
        popover_list.set_filter_func(move |row| {
            snippets::matches(&snippets::all()[row.index() as usize], &q.borrow())
        });
        let list = popover_list.clone();
        let q = query.clone();
        self.search.connect_search_changed(move |entry| {
            *q.borrow_mut() = entry.text().to_string();
            list.invalidate_filter();
        });
        let state = self.clone();
        self.search.connect_activate(move |entry| {
            let text = entry.text();
            if let Some(snippet) = snippets::all().iter().find(|s| snippets::matches(s, &text)) {
                state.insert_popover.popdown();
                state.insert_snippet(snippet);
            }
        });
        let popover = self.insert_popover.clone();
        self.search.connect_stop_search(move |_| popover.popdown());
        let search = self.search.clone();
        self.insert_popover.connect_show(move |_| {
            search.set_text("");
            search.grab_focus();
        });

        // Find bar: search as you type, Enter/Shift+Enter step through
        // matches, Escape closes and returns focus to the main window.
        let state = self.clone();
        self.find_entry.connect_search_changed(move |_| state.find_incremental());
        let state = self.clone();
        self.find_entry.connect_activate(move |_| state.find_next(false));
        let state = self.clone();
        self.find_entry.connect_next_match(move |_| state.find_next(false));
        let state = self.clone();
        self.find_entry.connect_previous_match(move |_| state.find_next(true));
        let state = self.clone();
        self.find_entry.connect_stop_search(move |_| {
            state.search_bar.set_search_mode(false);
        });
        let find_key = gtk4::EventControllerKey::new();
        let state = self.clone();
        find_key.connect_key_pressed(move |_, keyval, _, modifiers| {
            let enter = matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter);
            if enter && modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
                state.find_next(true);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.find_entry.add_controller(find_key);
        // Covers Escape and the bar's own close button alike.
        let state = self.clone();
        self.search_bar.connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode() {
                let (buffer, view) = state.find_target();
                // Hand the current match over as a plain selection, ready to
                // type over; the highlight tags go away with the bar.
                if !state.find_entry.text().is_empty() {
                    if let (Some(s), Some(e)) = (buffer.mark("find-start"), buffer.mark("find-end"))
                    {
                        let (start, end) = (buffer.iter_at_mark(&s), buffer.iter_at_mark(&e));
                        if start != end {
                            buffer.select_range(&start, &end);
                        }
                    }
                }
                state.find_clear_highlight();
                view.grab_focus();
            }
        });

        // Smart Enter/Backspace: continue or stop lists and table rows.
        let key = gtk4::EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let state = self.clone();
        key.connect_key_pressed(move |_, keyval, _, modifiers| {
            if modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SHIFT_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            if !state.doc_kind().smart_edit() {
                return glib::Propagation::Proceed;
            }
            match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => state.handle_enter(),
                gdk::Key::BackSpace => state.handle_backspace(),
                _ => glib::Propagation::Proceed,
            }
        });
        self.editor.add_controller(key);

        // Pasting an image (a screenshot) or image files copied in Files
        // stores them with the document, as inserting them would. A
        // clipboard that also offers text keeps the ordinary text paste.
        let state = self.clone();
        self.editor.connect_paste_clipboard(move |view| {
            if !state.doc_kind().is_markdown() {
                return;
            }
            let clipboard = view.clipboard();
            let formats = clipboard.formats();
            if formats.contains_type(gdk::FileList::static_type()) {
                view.stop_signal_emission_by_name("paste-clipboard");
                state.paste_files(clipboard);
            } else if formats.contains_type(gdk::Texture::static_type())
                && !formats.contains_type(String::static_type())
            {
                view.stop_signal_emission_by_name("paste-clipboard");
                state.paste_image(clipboard);
            }
        });

        // Editor cursor movement (clicks, arrow keys, selecting) keeps the
        // preview centered on the same spot, not just typing.
        let state = self.clone();
        self.buffer.connect_cursor_position_notify(move |_| {
            if state.loading.get() || state.syncing_cursor.get() {
                return;
            }
            state.center_preview_on_cursor();
        });

        // Click / word-select in the preview moves the editor cursor there.
        // Deferred to an idle so the preview TextView has already handled the
        // press (double-click word selection in particular). Beside a line
        // instead of on it, a double-click picks that line as the gutter
        // does, and Shift+click extends the pick.
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_PRIMARY);
        let state = self.clone();
        click.connect_pressed(move |gesture, n_press, x, y| {
            let shift = gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            let beside = state.preview_line_beside(x, y);
            if let Some(lines) = beside.filter(|_| shift || n_press == 2) {
                let state = state.clone();
                glib::idle_add_local_once(move || state.pick_from_preview(lines, shift));
                return;
            }
            // Glyphs and links act only when hit, not as the nearest character.
            if let Some(offset) = state.preview_offset_at(x, y).filter(|_| beside.is_none()) {
                // Ctrl+click on a link opens it with the system default app.
                if gesture.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                    if let Some(dest) = state.preview_link_at(offset) {
                        state.open_link(&dest);
                        return;
                    }
                }
                // A click on a task list checkbox ticks it in the source.
                if let Some(checkbox) = state.preview_checkbox_at(offset) {
                    state.toggle_task(&checkbox);
                    return;
                }
                // A click on a heading's disclosure marker folds its section.
                if let Some(key) = state.preview_fold_at(offset) {
                    state.toggle_fold(key);
                    return;
                }
            }
            let state = state.clone();
            glib::idle_add_local_once(move || state.sync_editor_to_preview(x, y));
        });
        self.preview.add_controller(click);

        // Right-click on an image: "Copy Image" (one popover, refilled per
        // click, as in the Files panel). On a link: save a copy of the
        // referenced file. An image inside a link gets the menu.
        let image_menu = gio::Menu::new();
        let image_popover = gtk4::PopoverMenu::from_model(Some(&image_menu));
        image_popover.set_parent(&self.preview);
        image_popover.set_has_arrow(false);
        image_popover.set_halign(gtk4::Align::Start);
        let rclick = gtk4::GestureClick::new();
        rclick.set_button(gdk::BUTTON_SECONDARY);
        let state = self.clone();
        rclick.connect_pressed(move |_, _, x, y| {
            let Some(offset) = state.preview_offset_at(x, y) else { return };
            if let Some(dest) = state.preview_image_at(offset) {
                image_menu.remove_all();
                let item = gio::MenuItem::new(Some("_Copy Image"), None);
                item.set_action_and_target_value(Some("win.copy-image"), Some(&dest.to_variant()));
                image_menu.append_item(&item);
                image_popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                image_popover.popup();
            } else if let Some(dest) = state.preview_link_at(offset) {
                state.save_attachment_dialog(&dest);
            }
        });
        self.preview.add_controller(rclick);

        // Ctrl+C on a preview selection that is just an image copies the
        // image itself; anything else copies text as before.
        let state = self.clone();
        self.preview.connect_copy_clipboard(move |view| {
            if let Some(dest) = state.selected_preview_image() {
                view.stop_signal_emission_by_name("copy-clipboard");
                state.copy_image(&dest);
            }
        });

        // Hand cursor and tooltip while hovering a link or checkbox in the preview.
        let motion = gtk4::EventControllerMotion::new();
        let state = self.clone();
        motion.connect_motion(move |_, x, y| {
            let offset = state.preview_offset_at(x, y);
            if let Some(checkbox) = offset.and_then(|o| state.preview_checkbox_at(o)) {
                state.preview.set_cursor_from_name(Some("pointer"));
                state.preview.set_tooltip_text(Some(if checkbox.checked {
                    "Click to uncheck"
                } else {
                    "Click to check"
                }));
                return;
            }
            let link = offset.and_then(|o| state.preview_link_at(o));
            state
                .preview
                .set_cursor_from_name(Some(if link.is_some() { "pointer" } else { "text" }));
            let tip = link.map(|dest| {
                if external_link(&dest) || dest.starts_with('#') {
                    format!("Ctrl+click to open {dest}")
                } else {
                    format!("Ctrl+click to open, right-click to save a copy — {dest}")
                }
            });
            state.preview.set_tooltip_text(tip.as_deref());
        });
        self.preview.add_controller(motion);

        // Banner "Load": allow remote images for this document only.
        let state = self.clone();
        self.remote_banner.connect_button_clicked(move |banner| {
            state.doc_allows_remote.set(true);
            banner.set_revealed(false);
            state.render_preview(true);
        });

        // Edits made since the banner appeared still get the usual question.
        let state = self.clone();
        self.disk_banner.connect_button_clicked(move |_| {
            let Some(path) = state.source.borrow().container().map(Path::to_path_buf) else {
                return;
            };
            let s = state.clone();
            state.guard_unsaved(Box::new(move || s.open_path(&path)));
        });
        // Coming back to the window is when a change made elsewhere matters.
        let state = self.clone();
        self.window.connect_is_active_notify(move |window| {
            if window.is_active() {
                state.check_disk_changes();
            }
        });

        let state = self.clone();
        self.files_list.connect_row_activated(move |_, row| {
            let path = usize::try_from(row.index())
                .ok()
                .and_then(|i| state.files_order.borrow().get(i).cloned());
            // The click already moved the selection; put it back on the open
            // document until the open (if it gets past guard_unsaved) lands.
            state.mark_current_file();
            let Some(path) = path else { return };
            if state.source.borrow().container() == Some(path.as_path()) {
                return;
            }
            let s = state.clone();
            state.guard_unsaved(Box::new(move || s.open_path(&path)));
        });
        // Right-click: "Open Location", "Remove from List". One popover, refilled per click —
        // unparenting a fresh one on `closed` can drop the action it fires.
        let files_menu = gio::Menu::new();
        let files_popover = gtk4::PopoverMenu::from_model(Some(&files_menu));
        files_popover.set_parent(&self.files_list);
        files_popover.set_has_arrow(false);
        files_popover.set_halign(gtk4::Align::Start);
        let click = gtk4::GestureClick::new();
        click.set_button(gdk::BUTTON_SECONDARY);
        let state = self.clone();
        click.connect_pressed(move |_, _, x, y| {
            let Some(row) = state.files_list.row_at_y(y as i32) else { return };
            let Some(path) = usize::try_from(row.index())
                .ok()
                .and_then(|i| state.files_order.borrow().get(i).cloned())
            else {
                return;
            };
            files_menu.remove_all();
            let item = gio::MenuItem::new(Some("Open _Location"), None);
            item.set_action_and_target_value(
                Some("win.open-location"),
                Some(&path.to_string_lossy().to_variant()),
            );
            files_menu.append_item(&item);
            let item = gio::MenuItem::new(Some("_Remove from List"), None);
            item.set_action_and_target_value(
                Some("win.forget-recent"),
                Some(&path.to_string_lossy().to_variant()),
            );
            files_menu.append_item(&item);
            files_popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            files_popover.popup();
        });
        self.files_list.add_controller(click);

        // Close: persist layout, confirm if there are unsaved changes.
        let state = self.clone();
        self.window.connect_close_request(move |_| {
            state.persist_settings();
            if state.modified.get() || state.memo_unpushed.get() {
                // Two guards in sequence: unsaved file changes first, then
                // memo changes that exist only on this computer.
                let unpushed = state.clone();
                let destroy = state.clone();
                state.guard_unsaved(Box::new(move || {
                    unpushed.guard_unpushed(Box::new(move || destroy.window.destroy()))
                }));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    fn register_actions(self: &Rc<Self>) {
        let actions: [(&str, Box<dyn Fn(&Rc<App>)>); 19] = [
            ("memos-send", Box::new(|s| s.memos_send_dialog())),
            ("memos-browse", Box::new(|s| s.memos_browse_dialog())),
            ("memos-push", Box::new(|s| s.memos_push(None))),
            ("memos-setup", Box::new(|s| s.memos_setup_dialog())),
            ("new", Box::new(|s| s.new_file())),
            ("find", Box::new(|s| s.show_find())),
            ("fold-all", Box::new(|s| s.toggle_fold_all())),
            ("open", Box::new(|s| s.open_dialog())),
            ("open-bundle", Box::new(|s| s.open_bundle_dialog())),
            ("save", Box::new(|s| s.save(None))),
            ("save-as", Box::new(|s| s.save_as(None))),
            ("export-pack", Box::new(|s| s.export_pack_dialog())),
            ("export-html", Box::new(|s| s.export_html_dialog())),
            ("export-pdf", Box::new(|s| s.export_pdf_dialog())),
            ("print", Box::new(|s| s.print_dialog())),
            ("export-odt", Box::new(|s| s.export_odt_dialog())),
            ("insert", Box::new(|s| if !s.typing_elsewhere() { s.show_insert_popover() })),
            ("bold", Box::new(|s| if !s.typing_elsewhere() { s.insert_named("Bold") })),
            ("italic", Box::new(|s| if !s.typing_elsewhere() { s.insert_named("Italic") })),
        ];
        for (name, handler) in actions {
            let action = gio::SimpleAction::new(name, None);
            let state = self.clone();
            action.connect_activate(move |_, _| handler(&state));
            self.window.add_action(&action);
        }

        let action = gio::SimpleAction::new("open-recent", Some(glib::VariantTy::STRING));
        let state = self.clone();
        action.connect_activate(move |_, param| {
            let Some(path) = param.and_then(|v| v.str()).map(PathBuf::from) else {
                return;
            };
            let s = state.clone();
            state.guard_unsaved(Box::new(move || s.open_path(&path)));
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("copy-image", Some(glib::VariantTy::STRING));
        let state = self.clone();
        action.connect_activate(move |_, param| {
            if let Some(dest) = param.and_then(|v| v.str()) {
                state.copy_image(dest);
            }
        });
        self.window.add_action(&action);

        // Show the file selected in the file manager (OpenURI portal, so it
        // works under Flatpak). An unmounted share or a vanished folder fails
        // here, asynchronously, rather than via a stat on the main loop.
        let action = gio::SimpleAction::new("open-location", Some(glib::VariantTy::STRING));
        let state = self.clone();
        action.connect_activate(move |_, param| {
            let Some(path) = param.and_then(|v| v.str()).map(PathBuf::from) else {
                return;
            };
            let s = state.clone();
            gtk4::FileLauncher::new(Some(&gio::File::for_path(&path))).open_containing_folder(
                Some(&state.window),
                gio::Cancellable::NONE,
                move |res| {
                    if let Err(e) = res {
                        if !e.matches(gtk4::DialogError::Dismissed) {
                            s.toast(&format!("Could not open the folder: {}", e.message()));
                        }
                    }
                },
            );
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("forget-recent", Some(glib::VariantTy::STRING));
        let state = self.clone();
        action.connect_activate(move |_, param| {
            let Some(path) = param.and_then(|v| v.str()).map(PathBuf::from) else {
                return;
            };
            state.recent_files.borrow_mut().retain(|p| *p != path);
            state.frozen_files.borrow_mut().retain(|p| *p != path);
            state.update_recent_menu();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful("view-mode", None, &false.to_variant());
        let state = self.clone();
        action.connect_activate(move |_, _| {
            // Via set_view_mode, which owns the action state and refuses the
            // switch for kinds with no rendered preview.
            state.set_view_mode(!state.view_mode.get());
            // Capture the new mode in the document's remembered state.
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "open-in-view",
            None,
            &self.open_in_view.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.open_in_view.get();
            action.set_state(&enabled.to_variant());
            state.open_in_view.set(enabled);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "open-last",
            None,
            &self.open_last.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.open_last.get();
            action.set_state(&enabled.to_variant());
            state.open_last.set(enabled);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "remember-folds",
            None,
            &self.remember_folds.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.remember_folds.get();
            action.set_state(&enabled.to_variant());
            state.remember_folds.set(enabled);
            // Turning it off forgets everything, not just future state.
            if !enabled {
                state.doc_state.borrow_mut().clear();
            }
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "multiple-instances",
            None,
            &self.multiple_instances.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.multiple_instances.get();
            action.set_state(&enabled.to_variant());
            state.multiple_instances.set(enabled);
            state.persist_settings();
            if enabled {
                state.toast("New launches will open their own window");
            }
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "follow-cursor",
            None,
            &self.follow_cursor.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.follow_cursor.get();
            action.set_state(&enabled.to_variant());
            state.follow_cursor.set(enabled);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "keyword-highlight",
            None,
            &self.keyword_highlight.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.keyword_highlight.get();
            action.set_state(&enabled.to_variant());
            state.keyword_highlight.set(enabled);
            state.persist_settings();
            state.apply_highlight();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "line-numbers",
            None,
            &self.line_numbers.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.line_numbers.get();
            action.set_state(&enabled.to_variant());
            state.line_numbers.set(enabled);
            state.apply_line_numbers(enabled);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "preview-line-numbers",
            None,
            &self.preview_line_numbers.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.preview_line_numbers.get();
            action.set_state(&enabled.to_variant());
            state.preview_line_numbers.set(enabled);
            state.apply_preview_line_numbers(enabled);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "word-wrap",
            None,
            &self.word_wrap.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.word_wrap.get();
            action.set_state(&enabled.to_variant());
            state.word_wrap.set(enabled);
            state.apply_wrap();
            state.refit_tables();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "editor-font",
            Some(glib::VariantTy::STRING),
            &self.editor_font.get().as_str().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, parameter| {
            let Some(choice) = parameter.and_then(|p| p.str().map(str::to_string)) else {
                return;
            };
            action.set_state(&choice.to_variant());
            state.editor_font.set(FontSize::from_str(&choice));
            state.apply_fonts();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "preview-font",
            Some(glib::VariantTy::STRING),
            &self.preview_font.get().as_str().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, parameter| {
            let Some(choice) = parameter.and_then(|p| p.str().map(str::to_string)) else {
                return;
            };
            action.set_state(&choice.to_variant());
            state.preview_font.set(FontSize::from_str(&choice));
            state.apply_fonts();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "preview-fit",
            None,
            &self.preview_fit.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let enabled = !state.preview_fit.get();
            action.set_state(&enabled.to_variant());
            state.preview_fit.set(enabled);
            state.apply_fonts();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "show-files",
            None,
            &self.show_files.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let shown = !state.show_files.get();
            action.set_state(&shown.to_variant());
            state.show_files.set(shown);
            state.apply_panels();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "show-insert",
            None,
            &self.show_insert.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let shown = !state.show_insert.get();
            action.set_state(&shown.to_variant());
            state.show_insert.set(shown);
            state.apply_panels();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "show-preview",
            None,
            &self.show_preview.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let shown = !state.show_preview.get();
            action.set_state(&shown.to_variant());
            state.show_preview.set(shown);
            state.apply_panels();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "horizontal",
            None,
            &self.horizontal.get().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, _| {
            let horizontal = !state.horizontal.get();
            action.set_state(&horizontal.to_variant());
            // Remember this layout's divider positions and start the other
            // one with its remembered positions, or sensible defaults.
            let previous =
                (state.hpaned.position(), state.lower.position(), state.sidebar.position());
            state.apply_horizontal(horizontal);
            let (h, insert, files) = state.saved_panes.replace(Some(previous)).unwrap_or(
                if horizontal {
                    // max() guards against an unallocated window (size 0).
                    (
                        (state.window.height() * 3 / 5).max(300),
                        (state.window.width() / 4).max(200),
                        (state.window.width() / 4).max(200),
                    )
                } else {
                    (300, 320, 200)
                },
            );
            state.hpaned.set_position(h);
            state.lower.set_position(insert);
            state.sidebar.set_position(files);
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "remote-images",
            Some(glib::VariantTy::STRING),
            &self.remote_policy.get().as_str().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, parameter| {
            let Some(choice) = parameter.and_then(|p| p.str().map(str::to_string)) else {
                return;
            };
            action.set_state(&choice.to_variant());
            state.remote_policy.set(RemotePolicy::from_str(&choice));
            state.persist_settings();
            state.render_preview(true);
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "color-scheme",
            Some(glib::VariantTy::STRING),
            &self.color_scheme.get().as_str().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, parameter| {
            let Some(choice) = parameter.and_then(|p| p.str().map(str::to_string)) else {
                return;
            };
            action.set_state(&choice.to_variant());
            let scheme = ColorScheme::from_str(&choice);
            state.color_scheme.set(scheme);
            apply_color_scheme(scheme);
            // ColorScheme::System may leave `is_dark` exactly where it was, in
            // which case connect_dark_notify never fires — repaint directly
            // rather than relying on it.
            state.apply_theme();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new_stateful(
            "theme",
            Some(glib::VariantTy::STRING),
            &self.theme.get().as_str().to_variant(),
        );
        let state = self.clone();
        action.connect_activate(move |action, parameter| {
            let Some(choice) = parameter.and_then(|p| p.str().map(str::to_string)) else {
                return;
            };
            action.set_state(&choice.to_variant());
            state.theme.set(Theme::from_str(&choice));
            state.apply_theme();
            state.persist_settings();
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("about", None);
        let window = self.window.clone();
        action.connect_activate(move |_, _| {
            let about = adw::AboutDialog::builder()
                .application_name("Bindle")
                .application_icon("no.oyzmo.Bindle")
                .version(env!("CARGO_PKG_VERSION"))
                // The "What's New" row: every release since the changelog
                // started, from the metainfo that bump.sh writes.
                .release_notes_version(env!("CARGO_PKG_VERSION"))
                .release_notes(crate::changelog::about_markup())
                .comments(
                    "A simple, fast Markdown editor for GNOME. The live \
                     preview is drawn with native GTK widgets instead of a \
                     web view, clicking either side moves the cursor in the \
                     other, and lists and tables keep themselves tidy while \
                     you type.\n\n\
                     Documents can be plain .md files or TextBundle/TextPack \
                     packages carrying their own images and attachments, and \
                     export to HTML or PDF. Bindle also opens plain text, NFO \
                     art, JSON, YAML, TOML, shell scripts and XML with keyword \
                     highlighting.\n\n\
                     It can connect to a self-hosted Memos server to send, \
                     browse, edit and push back memos — the only network \
                     feature, and never used unless asked.",
                )
                .developer_name("oyzmo")
                .copyright("Made by oyzmo 2026 together with AI")
                .website("https://oyzmo.no/apps")
                .license_type(gtk4::License::Gpl30)
                // The GPL text is what License::Gpl30 renders; the crates
                // compiled in keep their own licences, and both files ship
                // with the app (share/licenses/no.oyzmo.Bindle/).
                .build();
            about.present(Some(&window));
        });
        self.window.add_action(&action);
    }

    /// A text field other than the editor has the keyboard: the find bar, a
    /// dialog's entry. The editing shortcuts are window-wide accelerators, so
    /// without this Ctrl+B typed there inserted **bold text** into the
    /// document hidden behind it. Entries focus an inner `GtkText`.
    fn typing_elsewhere(&self) -> bool {
        gtk4::prelude::GtkWindowExt::focus(&self.window).is_some_and(|w| w.is::<gtk4::Text>())
    }

    // --- Smart editing ---

    /// Text of the cursor's line before and after the cursor, or None when
    /// a selection is active (smart editing then stays out of the way).
    fn line_around_cursor(&self) -> Option<(String, String)> {
        if self.buffer.has_selection() {
            return None;
        }
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let mut line_start = cursor;
        line_start.set_line_offset(0);
        let mut line_end = cursor;
        if !line_end.ends_line() {
            line_end.forward_to_line_end();
        }
        Some((
            self.buffer.text(&line_start, &cursor, false).to_string(),
            self.buffer.text(&cursor, &line_end, false).to_string(),
        ))
    }

    /// Delete everything on the cursor's line before the cursor — or, with
    /// `whole`, the line's entire text (its newline stays).
    fn clear_line(&self, whole: bool) {
        self.buffer.begin_user_action();
        let mut cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let mut line_start = cursor;
        line_start.set_line_offset(0);
        if whole && !cursor.ends_line() {
            cursor.forward_to_line_end();
        }
        self.buffer.delete(&mut line_start, &mut cursor);
        self.buffer.end_user_action();
    }

    fn handle_enter(&self) -> glib::Propagation {
        let Some((before, after)) = self.line_around_cursor() else {
            return glib::Propagation::Proceed;
        };
        match editor::on_enter(&before, &after) {
            editor::EnterAction::None => return glib::Propagation::Proceed,
            editor::EnterAction::Continue(text) => {
                self.buffer.begin_user_action();
                self.buffer.insert_at_cursor(&text);
                self.buffer.end_user_action();
            }
            editor::EnterAction::ContinueTable { text, cursor_back } => {
                self.buffer.begin_user_action();
                self.buffer.insert_at_cursor(&text);
                let mut iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
                iter.backward_chars(cursor_back);
                self.buffer.place_cursor(&iter);
                self.buffer.end_user_action();
            }
            editor::EnterAction::StopEmpty => self.clear_line(false),
            editor::EnterAction::ClearLine => self.clear_line(true),
        }
        self.editor.scroll_mark_onscreen(&self.buffer.get_insert());
        glib::Propagation::Stop
    }

    fn handle_backspace(&self) -> glib::Propagation {
        let Some((before, after)) = self.line_around_cursor() else {
            return glib::Propagation::Proceed;
        };
        match editor::on_backspace(&before, &after) {
            editor::BackspaceAction::None => return glib::Propagation::Proceed,
            editor::BackspaceAction::ClearBefore => self.clear_line(false),
            editor::BackspaceAction::ClearLine => self.clear_line(true),
        }
        glib::Propagation::Stop
    }

    // --- Inserting ---

    fn insert_snippet(self: &Rc<Self>, snippet: &Snippet) {
        match snippet.kind {
            snippets::Kind::ImagePicker => return self.insert_image_dialog(),
            snippets::Kind::WebImage => return self.insert_web_image_dialog(),
            snippets::Kind::LinkFile => return self.link_file_dialog(),
            snippets::Kind::AttachFile => return self.attach_file_dialog(),
            _ => {}
        }
        snippets::apply(&self.buffer, snippet);
        self.editor.grab_focus();
        self.editor.scroll_mark_onscreen(&self.buffer.get_insert());
    }

    fn insert_named(self: &Rc<Self>, label: &str) {
        if let Some(snippet) = snippets::all().iter().find(|s| s.label == label) {
            self.insert_snippet(snippet);
        }
    }

    fn insert_image_dialog(self: &Rc<Self>) {
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        let images = gtk4::FileFilter::new();
        images.set_name(Some("Images"));
        images.add_pixbuf_formats();
        filters.append(&images);
        let dialog = gtk4::FileDialog::builder()
            .title("Insert Image")
            .filters(&filters)
            .build();
        let state = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                state.insert_image_markdown(&path);
            }
        });
    }

    /// Insert `![alt](path)` for the picked file, with the alt text
    /// preselected for editing. Plain documents get a document-relative path;
    /// bundles get the image copied into `assets/`; packs stage the bytes in
    /// memory until the next save.
    fn insert_image_markdown(&self, path: &Path) {
        if let Some((markdown, alt)) = self.image_markdown(path) {
            self.insert_image_reference(&markdown, &alt);
        }
    }

    /// The `![alt](dest)` reference for `path` and its alt text, after
    /// copying or staging the image as the document kind requires; None
    /// (with a toast) when that failed.
    fn image_markdown(&self, path: &Path) -> Option<(String, String)> {
        let alt = markdown_label(
            &path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "image".to_string()),
        );
        let source = self.source.borrow().clone();
        let dest = match &source {
            DocSource::Bundle { dir, .. } => {
                match bundle::copy_into_assets(path, &dir.join("assets")) {
                    Ok(name) => format!("assets/{name}"),
                    Err(err) => {
                        self.toast(&format!("Could not copy image into bundle: {err}"));
                        return None;
                    }
                }
            }
            DocSource::Pack { info, .. } => self.stage_pack_asset(info, path),
            _ => {
                let target = match source.preview_base() {
                    Some(dir) => relative_to(&dir, path),
                    None => path.to_path_buf(),
                };
                target.display().to_string()
            }
        };
        Some((format!("![{alt}]({})", markdown_dest(&dest)), alt))
    }

    /// Ask for an image URL and insert `![alt](url)`.
    fn insert_web_image_dialog(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Insert Web Image"),
            Some("Paste or type the image URL"),
        );
        let entry = gtk4::Entry::new();
        entry.set_placeholder_text(Some("https://example.com/picture.jpg"));
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", "_Cancel"), ("insert", "_Insert")]);
        dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("insert"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        let url_entry = entry.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response.as_str() == "insert" {
                if let Some((md, alt)) = snippets::web_image_markdown(&url_entry.text()) {
                    state.insert_image_reference(&md, &alt);
                }
            }
        });
        entry.grab_focus();
    }

    /// Insert an image markdown snippet with the alt text preselected.
    fn insert_image_reference(&self, markdown: &str, alt: &str) {
        self.insert_reference(markdown, 2, alt.chars().count() as i32);
    }

    /// Insert markdown at the cursor with the label at [sel_start,
    /// sel_start+sel_len) preselected so typing replaces it.
    fn insert_reference(&self, markdown: &str, sel_start: i32, sel_len: i32) {
        self.buffer.begin_user_action();
        let mut iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let offset = iter.offset();
        self.buffer.insert(&mut iter, markdown);
        let start = self.buffer.iter_at_offset(offset + sel_start);
        let end = self.buffer.iter_at_offset(offset + sel_start + sel_len);
        self.buffer.select_range(&start, &end);
        self.buffer.end_user_action();
        self.editor.grab_focus();
        self.editor.scroll_mark_onscreen(&self.buffer.get_insert());
    }

    /// Pick a file and insert `[name](relative path)` — a plain link to the
    /// file where it lives; exports only bundle it if it stays reachable.
    fn link_file_dialog(self: &Rc<Self>) {
        let dialog = gtk4::FileDialog::builder().title("Link File").build();
        let state = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                let name = markdown_label(&file_label(&path));
                let target = match state.source.borrow().preview_base() {
                    Some(dir) => relative_to(&dir, &path),
                    None => path.clone(),
                };
                let dest = markdown_dest(&target.display().to_string());
                state.insert_reference(
                    &format!("[{name}]({dest})"),
                    1,
                    name.chars().count() as i32,
                );
            }
        });
    }

    /// Pick a file and make it part of the document: copied into the
    /// bundle's / the document's `assets/` directory (staged in memory for
    /// packs) and inserted as `[name](assets/…)`.
    fn attach_file_dialog(self: &Rc<Self>) {
        let dialog = gtk4::FileDialog::builder().title("Attach File").build();
        let state = self.clone();
        dialog.open(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                state.attach_file(&path);
            }
        });
    }

    fn attach_file(&self, path: &Path) {
        let name = markdown_label(&file_label(&path.to_path_buf()));
        let source = self.source.borrow().clone();
        let assets_dir = match &source {
            DocSource::Bundle { dir, .. } => dir.join("assets"),
            DocSource::Plain(doc) => match doc.parent() {
                Some(dir) => dir.join("assets"),
                None => return self.toast("The document has no folder to attach files in"),
            },
            DocSource::Pack { info, .. } => {
                let dest = self.stage_pack_asset(info, path);
                return self.insert_attachment_link(&name, dest);
            }
            DocSource::None | DocSource::Odt { .. } => {
                return self.toast("Save the document first, then attach files to it");
            }
        };
        match bundle::copy_into_assets(path, &assets_dir) {
            Ok(stored) => self.insert_attachment_link(&name, format!("assets/{stored}")),
            Err(err) => self.toast(&format!("Could not copy the file: {err}")),
        }
    }

    /// `[name](dest)` with the label preselected. The label stays the file's
    /// own name even when the stored copy had to be renumbered. `name` is
    /// already escaped (`markdown_label`).
    fn insert_attachment_link(&self, name: &str, dest: String) {
        let dest = markdown_dest(&dest);
        self.insert_reference(&format!("[{name}]({dest})"), 1, name.chars().count() as i32);
    }

    /// Stage `file` for the pack's `assets/` and return its `assets/…`
    /// reference. Names already used — by an entry of the pack or by another
    /// staged file — are left alone, since saving would otherwise replace
    /// what other references point at; the same file staged twice keeps its
    /// first name.
    fn stage_pack_asset(&self, info: &bundle::PackInfo, file: &Path) -> String {
        let base = file_label(&file.to_path_buf());
        let asset = bundle::AssetSource::File(file.to_path_buf());
        self.stage_asset(&base, asset, |key| info.entries.contains(&format!("{}{key}", info.prefix)))
    }

    /// Stage `asset` as `assets/<base>` until the next save and return that
    /// reference, renumbered past names another staged asset or `exists`
    /// (the container's own entries) already use.
    fn stage_asset(&self, base: &str, asset: bundle::AssetSource, exists: impl Fn(&str) -> bool) -> String {
        let mut staged = self.staged_assets.borrow_mut();
        let name = bundle::unique_name(base, |candidate| {
            let key = format!("assets/{candidate}");
            match (staged.get(&key), &asset) {
                (Some(bundle::AssetSource::File(p)), bundle::AssetSource::File(file)) => {
                    !bundle::same_file(p, file)
                }
                (Some(_), _) => true,
                (None, _) => exists(&key),
            }
        });
        let key = format!("assets/{name}");
        staged.insert(key.clone(), asset);
        key
    }

    /// Paste the clipboard's image, read asynchronously (the clipboard may
    /// belong to another process), as a PNG stored with the document.
    fn paste_image(self: &Rc<Self>, clipboard: gdk::Clipboard) {
        let state = self.clone();
        glib::spawn_future_local(async move {
            match clipboard.read_texture_future().await {
                Ok(Some(texture)) => state.store_pasted_image(&texture.save_to_png_bytes()),
                Ok(None) => {}
                Err(err) => state.toast(&format!("Could not paste the image: {err}")),
            }
        });
    }

    /// Files copied in a file manager: image files are inserted as images
    /// (as Insert Image would). Anything else pastes the clipboard's text,
    /// which is what the editor would have done.
    fn paste_files(self: &Rc<Self>, clipboard: gdk::Clipboard) {
        let state = self.clone();
        glib::spawn_future_local(async move {
            let files = clipboard
                .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
                .await
                .ok()
                .and_then(|value| value.get::<gdk::FileList>().ok())
                .map(|list| list.files())
                .unwrap_or_default();
            let images: Vec<PathBuf> =
                files.iter().filter_map(|f| f.path()).filter(|p| is_image_file(p)).collect();
            if !images.is_empty() && images.len() < files.len() {
                let skipped = files.len() - images.len();
                state.toast(&format!("Pasted the images; skipped {skipped} file(s) that aren't"));
            }
            if images.is_empty() {
                if let Ok(Some(text)) = clipboard.read_text_future().await {
                    let editable = state.editor.is_editable();
                    state.buffer.delete_selection(true, editable);
                    state.buffer.insert_interactive_at_cursor(&text, editable);
                }
                return;
            }
            let refs: Vec<(String, String)> =
                images.iter().filter_map(|path| state.image_markdown(path)).collect();
            if let [(markdown, alt)] = refs.as_slice() {
                return state.paste_reference(markdown, 2, alt.chars().count() as i32);
            }
            let markdown = refs.iter().map(|(md, _)| md.as_str()).collect::<Vec<_>>().join("\n");
            state.paste_reference(&markdown, markdown.chars().count() as i32, 0);
        });
    }

    /// Store pasted PNG bytes where an inserted image of this document goes:
    /// a plain file's or bundle's `assets/` directly; a pack, an imported
    /// .odt or an unsaved document stage them in memory until it is saved
    /// (Save As then writes them out, see `write_to`).
    fn store_pasted_image(&self, png: &[u8]) {
        let name = glib::DateTime::now_local()
            .and_then(|now| now.format("pasted-%Y%m%d-%H%M%S.png"))
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "pasted.png".to_string());
        let source = self.source.borrow().clone();
        let assets_dir = match &source {
            DocSource::Bundle { dir, .. } => dir.join("assets"),
            DocSource::Plain(doc) => match doc.parent() {
                Some(dir) => dir.join("assets"),
                None => return self.toast("The document has no folder to store the image in"),
            },
            DocSource::Pack { info, .. } => {
                let asset = bundle::AssetSource::Bytes(png.to_vec());
                let dest = self.stage_asset(&name, asset, |key| {
                    info.entries.contains(&format!("{}{key}", info.prefix))
                });
                return self.paste_image_reference(&dest);
            }
            DocSource::None | DocSource::Odt { .. } => {
                let dest = self.stage_asset(&name, bundle::AssetSource::Bytes(png.to_vec()), |_| false);
                return self.paste_image_reference(&dest);
            }
        };
        match bundle::write_into_assets(&name, png, &assets_dir) {
            Ok(stored) => self.paste_image_reference(&format!("assets/{stored}")),
            Err(err) => self.toast(&format!("Could not store the image: {err}")),
        }
    }

    fn paste_image_reference(&self, dest: &str) {
        let alt = "image";
        let markdown = format!("![{alt}]({})", markdown_dest(dest));
        self.paste_reference(&markdown, 2, alt.len() as i32);
    }

    /// `insert_reference` replacing the selection, as a paste does — one
    /// undo step for both.
    fn paste_reference(&self, markdown: &str, sel_start: i32, sel_len: i32) {
        self.buffer.begin_user_action();
        self.buffer.delete_selection(true, self.editor.is_editable());
        self.insert_reference(markdown, sel_start, sel_len);
        self.buffer.end_user_action();
    }

    fn show_insert_popover(&self) {
        let iter = self.buffer.iter_at_mark(&self.buffer.get_insert());
        let loc = self.editor.iter_location(&iter);
        let (x, y) = self
            .editor
            .buffer_to_window_coords(gtk4::TextWindowType::Widget, loc.x(), loc.y());
        self.insert_popover.set_pointing_to(Some(&gdk::Rectangle::new(
            x,
            y,
            loc.width().max(2),
            loc.height().max(2),
        )));
        self.insert_popover.popup();
    }

    // --- Find ---

    fn show_find(&self) {
        self.search_bar.set_search_mode(true);
        // Reopening keeps the previous query selected so typing replaces it.
        self.find_entry.grab_focus();
    }

    /// Find operates on whatever occupies the main window: the preview in
    /// view mode, the editor otherwise.
    fn find_target(&self) -> (gtk4::TextBuffer, gtk4::TextView) {
        if self.view_mode.get() {
            (self.preview.buffer(), self.preview.clone())
        } else {
            (self.buffer.clone(), self.editor.clone())
        }
    }

    /// Select the first match at or after the current selection start, so
    /// typing more of a word keeps the same match. Wraps around.
    fn find_incremental(&self) {
        let query = self.find_entry.text();
        if query.is_empty() {
            self.find_entry.remove_css_class("error");
            self.find_clear_highlight();
            return;
        }
        let (buffer, _) = self.find_target();
        // find_select parks the cursor at the current match start, so typing
        // more of a word keeps the same match while it still fits.
        let from = buffer.iter_at_mark(&buffer.get_insert());
        self.find_select(&query, from, false);
    }

    /// Step to the match after (or before) the current one. Wraps around.
    fn find_next(&self, backward: bool) {
        let query = self.find_entry.text();
        if query.is_empty() {
            return;
        }
        let (buffer, _) = self.find_target();
        let insert = buffer.iter_at_mark(&buffer.get_insert());
        // Step relative to the current match while the cursor still sits on
        // it (find_select parks it at the match start); a cursor the user
        // moved elsewhere wins.
        let from = match (buffer.mark("find-start"), buffer.mark("find-end")) {
            (Some(s), Some(e)) if buffer.iter_at_mark(&s) == insert => {
                if backward {
                    insert
                } else {
                    buffer.iter_at_mark(&e)
                }
            }
            _ => insert,
        };
        self.find_select(&query, from, backward);
    }

    /// Case-insensitive search: highlights every match, marks the current
    /// one (first at/after `from`, or before it when backward; wraps around)
    /// in a stronger color, reveals it, parks the cursor at its start and
    /// updates the "current/total" counter. No match: error style on the
    /// entry.
    fn find_select(&self, query: &str, from: gtk4::TextIter, backward: bool) {
        let (buffer, view) = self.find_target();
        self.find_clear_highlight();
        // VISIBLE_ONLY skips text hidden by collapsed preview sections.
        let flags = gtk4::TextSearchFlags::CASE_INSENSITIVE
            | gtk4::TextSearchFlags::TEXT_ONLY
            | gtk4::TextSearchFlags::VISIBLE_ONLY;
        let mut matches: Vec<(i32, i32)> = Vec::new();
        let mut it = buffer.start_iter();
        while let Some((s, e)) = it.forward_search(query, flags, None) {
            matches.push((s.offset(), e.offset()));
            it = e;
        }
        if matches.is_empty() {
            self.find_entry.add_css_class("error");
            self.find_count.set_text("0/0");
            return;
        }
        self.find_entry.remove_css_class("error");
        let from_off = from.offset();
        let current = if backward {
            matches.iter().rposition(|&(s, _)| s < from_off).unwrap_or(matches.len() - 1)
        } else {
            matches.iter().position(|&(s, _)| s >= from_off).unwrap_or(0)
        };
        for (i, &(s, e)) in matches.iter().enumerate() {
            let name = if i == current { "find-current" } else { "find-match" };
            buffer.apply_tag_by_name(name, &buffer.iter_at_offset(s), &buffer.iter_at_offset(e));
        }
        let start = buffer.iter_at_offset(matches[current].0);
        let end = buffer.iter_at_offset(matches[current].1);
        match buffer.mark("find-start") {
            Some(m) => buffer.move_mark(&m, &start),
            None => {
                buffer.create_mark(Some("find-start"), &start, true);
            }
        }
        match buffer.mark("find-end") {
            Some(m) => buffer.move_mark(&m, &end),
            None => {
                buffer.create_mark(Some("find-end"), &end, false);
            }
        }
        buffer.place_cursor(&start);
        if let Some(m) = buffer.mark("find-start") {
            view.scroll_to_mark(&m, 0.0, true, 0.0, 0.5);
        }
        self.find_count.set_text(&format!("{}/{}", current + 1, matches.len()));
    }

    /// Remove find highlights from both buffers and blank the counter.
    fn find_clear_highlight(&self) {
        for buffer in [self.buffer.clone(), self.preview.buffer()] {
            let (start, end) = (buffer.start_iter(), buffer.end_iter());
            buffer.remove_tag_by_name("find-match", &start, &end);
            buffer.remove_tag_by_name("find-current", &start, &end);
        }
        self.find_count.set_text("");
    }

    // --- Preview ---

    fn schedule_preview(self: &Rc<Self>) {
        if let Some(id) = self.preview_pending.borrow_mut().take() {
            id.remove();
        }
        let state = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(150), move || {
            state.preview_pending.borrow_mut().take();
            state.refresh_now();
        });
        *self.preview_pending.borrow_mut() = Some(id);
    }

    /// Run the debounced refresh right away, cancelling the pending one —
    /// for edits made by clicking the preview, which should show at once.
    fn flush_preview(self: &Rc<Self>) {
        if let Some(id) = self.preview_pending.borrow_mut().take() {
            id.remove();
        }
        self.refresh_now();
    }

    /// The refresh behind `schedule_preview`: preview render + incremental highlight.
    fn refresh_now(self: &Rc<Self>) {
        // A panic escaping a GLib callback aborts the whole process, and
        // this runs the heaviest logic on every pause in typing. A bug
        // in it must cost one refresh, not the user's unsaved text. The
        // panic hook in main.rs has already logged it by the time we see it.
        let refreshed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Nothing to see while the preview panel is hidden;
            // `apply_panels` renders once when it comes back.
            if self.preview_visible() {
                self.render_preview(true);
            }
            self.apply_highlight();
        }));
        if refreshed.is_err() {
            // The baseline may describe tags that were never applied.
            self.reset_highlight();
            thread_local!(static WARNED: Cell<bool> = const { Cell::new(false) });
            if !WARNED.with(|w| w.replace(true)) {
                self.toast("Preview refresh failed — details in Bindle's crash.log");
            }
        }
    }

    /// How many monospace characters fit across the preview's text area —
    /// the width tables are wrapped to. None before the view has a size,
    /// and while "Word wrap" is off: then tables keep their natural width
    /// and the preview scrolls sideways.
    fn preview_table_cols(&self) -> Option<usize> {
        if !self.word_wrap.get() {
            return None;
        }
        let width = self.preview.visible_rect().width()
            - self.preview.left_margin()
            - self.preview.right_margin();
        if width <= 0 {
            return None;
        }
        // Measure a run of the table font itself (bold, like the header)
        // rather than trusting the font metrics' approximate width.
        const RUN: i32 = 40;
        let layout = self.preview.create_pango_layout(Some(&"0".repeat(RUN as usize)));
        let mut font = layout.context().font_description()?;
        font.set_family("monospace");
        font.set_weight(gtk4::pango::Weight::Bold);
        layout.set_font_description(Some(&font));
        let run = layout.pixel_size().0;
        if run <= 0 {
            return None;
        }
        Some((width * RUN / run).max(1) as usize - 1)
    }

    /// Re-fit tables after the preview's width or font changed. Skipped
    /// when nothing on screen is a table; the next render fits anyway.
    fn refit_tables(self: &Rc<Self>) {
        if self.preview_visible()
            && !self.preview_table_rows.borrow().is_empty()
            && self.preview_table_cols() != self.preview_table_cols.get()
        {
            self.schedule_preview();
        }
    }

    /// Re-render the preview. `keep_position` restores the scroll position
    /// afterwards; pass false on document load to show the top. The restore
    /// is synchronous (same main-loop iteration, before the next draw) — a
    /// deferred mark scroll visibly scans from the top on long documents
    /// while the fresh layout validates.
    fn render_preview(self: &Rc<Self>, keep_position: bool) {
        let adj = self.preview.vadjustment();
        let restore = match (&adj, keep_position) {
            (Some(adj), true) => adj.value(),
            _ => 0.0,
        };
        let text = self.text();
        let source = self.source.borrow().clone();
        let base = source.preview_base();
        // Pack and ODT assets are read lazily out of the zip (staged ones
        // from memory), never extracted to disk. An unsaved document needs
        // the loader too once it holds pasted images, which live only in
        // memory until the first save.
        let pack_ctx = source.archive();
        let pasted = matches!(source, DocSource::None) && !self.staged_assets.borrow().is_empty();
        let archive = pack_ctx.as_ref();
        let loader = (archive.is_some() || pasted).then(|| {
            move |rel: &str| {
                match self.staged_assets.borrow().get(rel) {
                    Some(bundle::AssetSource::Bytes(bytes)) => return Some(bytes.clone()),
                    Some(bundle::AssetSource::File(p)) => {
                        return bundle::read_file_capped(p, bundle::ASSET_MAX_BYTES);
                    }
                    None => {}
                }
                let Some((path, info)) = archive else {
                    // Nothing else of an unsaved document is relative.
                    let path = Path::new(rel);
                    return path
                        .is_absolute()
                        .then(|| bundle::read_file_capped(path, bundle::ASSET_MAX_BYTES))
                        .flatten();
                };
                if let Some(bytes) = self.pack_bytes_cache.borrow().get(rel) {
                    return Some(bytes.clone());
                }
                let bytes = bundle::read_asset(path, info, rel, bundle::ASSET_MAX_BYTES)?;
                self.pack_bytes_cache.borrow_mut().insert(rel.to_string(), bytes.clone());
                Some(bytes)
            }
        });
        let loader_ref: Option<&dyn Fn(&str) -> Option<Vec<u8>>> =
            loader.as_ref().map(|f| f as &dyn Fn(&str) -> Option<Vec<u8>>);
        let weak = Rc::downgrade(self);
        let refresh: Rc<dyn Fn()> = Rc::new(move || {
            if let Some(state) = weak.upgrade() {
                state.render_preview(true);
            }
        });
        let policy = self.remote_policy.get();
        let allow = match policy {
            RemotePolicy::Always => true,
            RemotePolicy::Ask => self.doc_allows_remote.get(),
            RemotePolicy::Never => false,
        };
        // Render into a scratch buffer (sharing the tag table) and splice
        // only the differing region into the live one: untouched lines keep
        // their layout, so re-rendering can't make the view blink.
        let live = self.preview.buffer();
        let scratch = gtk4::TextBuffer::new(Some(&live.tag_table()));
        let kind = self.doc_kind();
        let table_cols = self.preview_table_cols();
        self.preview_table_cols.set(table_cols);
        let rendered = if kind.is_markdown() {
            preview::render_fitted(
                &scratch,
                &text,
                base.as_deref(),
                allow,
                Some(refresh),
                loader_ref,
                table_cols,
            )
        } else {
            preview::render_plain(&scratch, &text, kind.wrap_lines())
        };
        // Collapsed sections: swap the heading's marker glyph to ▸ before
        // the splice (same char count, so section offsets stay valid) and
        // hide the section body on the live buffer afterwards. Hiding by
        // tag keeps the text in place, so source-map and link offsets hold.
        self.collapsed
            .borrow_mut()
            .retain(|key| rendered.sections.iter().any(|s| &s.key == key));
        {
            let collapsed = self.collapsed.borrow();
            for section in &rendered.sections {
                if collapsed.contains(&section.key) {
                    // Keep every tag the ▾ had, not just `dim`: a heading
                    // inside a blockquote also carries `quote`, and the diff
                    // in `splice` compares tags too.
                    let tags = scratch.iter_at_offset(section.marker_start).tags();
                    let tags: Vec<&gtk4::TextTag> = tags.iter().collect();
                    let mut a = scratch.iter_at_offset(section.marker_start);
                    let mut b = scratch.iter_at_offset(section.marker_start + 1);
                    scratch.delete(&mut a, &mut b);
                    let mut a = scratch.iter_at_offset(section.marker_start);
                    scratch.insert_with_tags(&mut a, "▸", &tags);
                }
            }
        }
        preview::splice(&live, &scratch);
        {
            let (start, end) = live.bounds();
            live.remove_tag_by_name("hidden", &start, &end);
            let collapsed = self.collapsed.borrow();
            let total = live.char_count();
            for (i, section) in rendered.sections.iter().enumerate() {
                if collapsed.contains(&section.key) {
                    // Outline semantics: hide only the section's direct body,
                    // up to the next heading of any level (sections are in
                    // document order) — subheadings stay visible.
                    let body_end = rendered
                        .sections
                        .get(i + 1)
                        .map(|next| next.marker_start)
                        .unwrap_or(section.end)
                        .min(section.end)
                        .min(total);
                    if body_end > section.content_start {
                        live.apply_tag_by_name(
                            "hidden",
                            &live.iter_at_offset(section.content_start),
                            &live.iter_at_offset(body_end),
                        );
                    }
                }
            }
        }
        *self.preview_map.borrow_mut() = rendered.source_map;
        *self.preview_links.borrow_mut() = rendered.links;
        *self.preview_images.borrow_mut() = rendered.images;
        *self.preview_checkboxes.borrow_mut() = rendered.checkboxes;
        *self.preview_sections.borrow_mut() = rendered.sections;
        *self.preview_table_rows.borrow_mut() = rendered.table_rows;
        // The splice dropped the pick's tag wherever it replaced text.
        if self.line_pick.get().is_some() {
            self.apply_line_pick();
        }
        self.preview_gutter.queue_draw();
        self.update_fold_btn();
        self.remote_banner.set_revealed(
            rendered.saw_remote && policy == RemotePolicy::Ask && !self.doc_allows_remote.get(),
        );
        if let Some(adj) = adj {
            adj.set_value(restore);
            // Hold the position through the relayout: height revalidation can
            // clamp or shift the value between now and the next few paints,
            // which shows as a one-frame blink. Tick callbacks run right
            // before each paint, so the drawn frames always have the value.
            let frames = Cell::new(0);
            self.preview.add_tick_callback(move |_, _| {
                adj.set_value(restore);
                frames.set(frames.get() + 1);
                if frames.get() < 3 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
            });
        }
    }

    /// Icon and sensitivity of the collapse/expand-all header button:
    /// ▸ when every section is collapsed, ▾ otherwise; insensitive when
    /// the document has no sections.
    fn update_fold_btn(&self) {
        let sections = self.preview_sections.borrow();
        let collapsed = self.collapsed.borrow();
        let all = !sections.is_empty() && sections.iter().all(|s| collapsed.contains(&s.key));
        self.fold_btn
            .set_icon_name(if all { "pan-end-symbolic" } else { "pan-down-symbolic" });
        self.fold_btn.set_sensitive(!sections.is_empty());
    }

    /// The (single) mark used for programmatic preview scrolling.
    fn preview_scroll_mark(&self, iter: &gtk4::TextIter) -> gtk4::TextMark {
        let buffer = self.preview.buffer();
        match buffer.mark("scroll-target") {
            Some(mark) => {
                buffer.move_mark(&mark, iter);
                mark
            }
            None => buffer.create_mark(Some("scroll-target"), iter, false),
        }
    }

    /// Scroll the preview so the spot being edited is vertically centered.
    fn center_preview_on_cursor(&self) {
        if !self.follow_cursor.get() {
            return;
        }
        let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert()).offset();
        let Some(offset) = self.preview_map.borrow().preview_offset(cursor) else {
            return;
        };
        let preview_buffer = self.preview.buffer();
        let mut iter = preview_buffer.iter_at_offset(offset.min(preview_buffer.char_count()));
        // Anchor at the line start: centers vertically on the right line
        // without scrolling horizontally (matters for wide tables).
        iter.set_line_offset(0);
        // For the top of the document, jump the adjustment directly: an
        // aligned scroll_to_mark queued before the fresh buffer is laid out
        // (i.e. right after a document load) ends up at the bottom.
        if iter.offset() == 0 {
            if let Some(adj) = self.preview.vadjustment() {
                adj.set_value(adj.lower());
            }
            return;
        }
        // Already comfortably visible? Leave the view alone: re-centering on
        // every keystroke makes the preview jump around while typing.
        let loc = self.preview.iter_location(&iter);
        let visible = self.preview.visible_rect();
        let margin = visible.height() / 6;
        if loc.y() >= visible.y() + margin
            && loc.y() + loc.height() <= visible.y() + visible.height() - margin
        {
            return;
        }
        // scroll_to_mark (not scroll_to_iter): it stays queued until the
        // freshly rendered buffer has been laid out.
        let mark = self.preview_scroll_mark(&iter);
        self.preview.scroll_to_mark(&mark, 0.0, true, 0.0, 0.5);
    }

    /// The preview char offset under a preview widget position, if it is over
    /// text. Callers hit-test links, checkboxes and fold markers against it.
    fn preview_offset_at(&self, x: f64, y: f64) -> Option<i32> {
        let (bx, by) = self
            .preview
            .window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
        Some(self.preview.iter_at_location(bx, by)?.offset())
    }

    /// The link at a preview char offset, if any.
    fn preview_link_at(&self, offset: i32) -> Option<String> {
        self.preview_links
            .borrow()
            .iter()
            .find(|link| offset >= link.start && offset < link.end)
            .map(|link| link.dest.clone())
    }

    /// The reference of the image thumbnail at a preview char offset, if any.
    fn preview_image_at(&self, offset: i32) -> Option<String> {
        // Rendered in document order, so sorted by `offset`.
        let images = self.preview_images.borrow();
        let i = images.partition_point(|image| image.offset < offset);
        images.get(i).filter(|image| image.offset == offset).map(|image| image.dest.clone())
    }

    /// The image the preview selection consists of: exactly one thumbnail,
    /// give or take surrounding whitespace.
    fn selected_preview_image(&self) -> Option<String> {
        let buffer = self.preview.buffer();
        let (start, end) = buffer.selection_bounds()?;
        let images = self.preview_images.borrow();
        let mut inside = images.iter().filter(|i| i.offset >= start.offset() && i.offset < end.offset());
        let image = inside.next()?;
        if inside.next().is_some() {
            return None;
        }
        let text = buffer.slice(&start, &end, true);
        text.chars().all(|c| c == '\u{FFFC}' || c.is_whitespace()).then(|| image.dest.clone())
    }

    /// Put the full-size original of an image reference on the clipboard —
    /// not the preview's thumbnail. Web images are only held as thumbnails,
    /// so they are refused rather than copied small.
    ///
    /// The file is read on a worker thread: the document may live on a
    /// share that hangs, and the original can be up to `ASSET_MAX_BYTES`.
    fn copy_image(self: &Rc<Self>, dest: &str) {
        if external_link(dest) {
            return self.toast("Web images can't be copied; open the link instead");
        }
        let Some(loc) = self.asset_loc(dest) else {
            return self.toast("Could not read the image");
        };
        let state = self.clone();
        glib::spawn_future_local(async move {
            let bytes = gio::spawn_blocking(move || loc.read(bundle::ASSET_MAX_BYTES))
                .await
                .ok()
                .flatten();
            let texture = bytes
                .and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok());
            match texture {
                Some(texture) => {
                    state.preview.clipboard().set_texture(&texture);
                    state.toast("Image copied");
                }
                None => state.toast("Could not read the image"),
            }
        });
    }

    /// The task list checkbox glyph at a preview char offset, if any.
    fn preview_checkbox_at(&self, offset: i32) -> Option<preview::Checkbox> {
        // Rendered in document order, so sorted by `start`.
        let boxes = self.preview_checkboxes.borrow();
        let i = boxes.partition_point(|c| c.start < offset);
        boxes.get(i).filter(|c| c.start == offset).copied()
    }

    /// Tick or untick a task by editing the markdown source, as an undoable
    /// user edit, then refresh at once rather than after the debounce. The
    /// source offset comes from the last render: while a refresh is still
    /// pending the text has changed since, and a deleted line can shift
    /// another task's `[ ]` under the stale offset — so that click does
    /// nothing. The bracket and its state are checked too, which covers a
    /// render that failed.
    fn toggle_task(self: &Rc<Self>, checkbox: &preview::Checkbox) {
        if self.preview_pending.borrow().is_some() {
            return;
        }
        let mut mark = self.buffer.iter_at_offset(checkbox.source);
        let mut end = mark;
        end.forward_chars(3);
        let replacement = match (self.buffer.text(&mark, &end, false).as_str(), checkbox.checked) {
            ("[ ]", false) => "x",
            ("[x]" | "[X]", true) => " ",
            _ => return,
        };
        mark.forward_char();
        let mut mark_end = mark;
        mark_end.forward_char();
        // Editing next to the cursor can move it; don't let that scroll the preview.
        self.syncing_cursor.set(true);
        self.buffer.begin_user_action();
        self.buffer.delete(&mut mark, &mut mark_end);
        self.buffer.insert(&mut mark, replacement);
        self.buffer.end_user_action();
        self.syncing_cursor.set(false);
        self.flush_preview();
    }

    /// The collapsible section whose disclosure marker sits at a preview
    /// widget position, if any.
    fn preview_fold_at(&self, offset: i32) -> Option<String> {
        self.preview_sections
            .borrow()
            .iter()
            .find(|s| offset >= s.marker_start && offset < s.marker_end)
            .map(|s| s.key.clone())
    }

    /// Collapse every section, or expand them all once everything is
    /// already collapsed.
    fn toggle_fold_all(self: &Rc<Self>) {
        {
            let sections = self.preview_sections.borrow();
            if sections.is_empty() {
                return;
            }
            let mut collapsed = self.collapsed.borrow_mut();
            if sections.iter().all(|s| collapsed.contains(&s.key)) {
                collapsed.clear();
            } else {
                collapsed.extend(sections.iter().map(|s| s.key.clone()));
            }
        }
        self.render_preview(true);
        self.persist_settings();
    }

    fn toggle_fold(self: &Rc<Self>, key: String) {
        {
            let mut collapsed = self.collapsed.borrow_mut();
            if !collapsed.remove(&key) {
                collapsed.insert(key);
            }
        }
        self.render_preview(true);
        self.persist_settings();
    }

    /// Move `path` to the front of the recent-files list.
    fn remember_recent(&self, path: &Path) {
        let path = path.to_path_buf();
        let mut recent = self.recent_files.borrow_mut();
        recent.retain(|p| *p != path);
        recent.insert(0, path);
        recent.truncate(RECENT_MAX);
    }

    /// Rebuild the "Open Recent" submenu from the recent-files list. No
    /// existence checks here: a stat on an unreachable FUSE/network mount
    /// can hang, and this runs on the main loop. Opening a gone entry
    /// fails with a toast instead.
    fn update_recent_menu(&self) {
        self.recent_menu.remove_all();
        for path in self.recent_files.borrow().iter() {
            let item = gio::MenuItem::new(Some(&file_label(path)), None);
            item.set_action_and_target_value(
                Some("win.open-recent"),
                Some(&path.to_string_lossy().to_variant()),
            );
            self.recent_menu.append_item(&item);
        }
        if self.recent_menu.n_items() == 0 {
            // Actionless item renders as an insensitive placeholder.
            self.recent_menu.append_item(&gio::MenuItem::new(Some("No recent documents"), None));
        }
        self.update_files_panel();
    }

    /// Rebuild the Files panel from the same list, under the same rule: no
    /// stat on the main loop, so a gone entry only says so when clicked.
    /// The rows keep `files_order`, reconciled with the recent list, with
    /// pinned files on top.
    fn update_files_panel(&self) {
        {
            let recent = self.recent_files.borrow();
            let frozen = self.frozen_files.borrow();
            let mut order = self.files_order.borrow_mut();
            order.retain(|p| recent.contains(p) || frozen.contains(p));
            for path in recent.iter().rev().chain(frozen.iter()) {
                if !order.contains(path) {
                    order.push(path.clone());
                }
            }
            *order = pinned_first(std::mem::take(&mut *order), &frozen);
        }
        // Not remove_all(): with a placeholder set, GTK's loop keeps trying
        // to remove the placeholder (a child, but not a row) forever.
        while let Some(row) = self.files_list.row_at_index(0) {
            self.files_list.remove(&row);
        }
        let recent = self.files_order.borrow();
        let names: Vec<String> = recent.iter().map(file_label).collect();
        for (path, name) in recent.iter().zip(&names) {
            let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            let label = gtk4::Label::new(Some(name));
            label.set_hexpand(true);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            hbox.append(&label);
            // Name only (the tooltip has the path), except where two entries
            // share a name: then the folder tells them apart.
            if names.iter().filter(|n| *n == name).count() > 1 {
                let folder = path.parent().and_then(Path::file_name).map(|f| f.to_string_lossy());
                if let Some(folder) = folder {
                    let suffix = gtk4::Label::new(Some(&format!("· {folder}")));
                    suffix.add_css_class("caption");
                    suffix.add_css_class("dim-label");
                    suffix.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    hbox.append(&suffix);
                    label.set_hexpand(false);
                    suffix.set_hexpand(true);
                    suffix.set_xalign(0.0);
                }
            }
            hbox.append(&self.freeze_button(path));
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&hbox));
            row.set_tooltip_text(Some(&path.display().to_string()));
            self.files_list.append(&row);
        }
        drop(recent);
        self.mark_current_file();
    }

    /// The Files row's freeze pin: a frozen file stays in the panel when it
    /// falls off the recent list. Shown dim on hover only until set.
    fn freeze_button(&self, path: &Path) -> gtk4::ToggleButton {
        let frozen = self.frozen_files.borrow().iter().any(|p| p == path);
        let button = gtk4::ToggleButton::builder()
            .icon_name("view-pin-symbolic")
            .active(frozen)
            .valign(gtk4::Align::Center)
            .tooltip_text(if frozen { "Unfreeze" } else { "Freeze in List" })
            .css_classes(["flat", "files-freeze"])
            .build();
        let path = path.to_path_buf();
        // From the thread-local at click time: this also runs while `build`
        // is still assembling the window, before there is an Rc to capture.
        button.connect_toggled(move |button| {
            let Some(state) = STATE.with(|s| s.borrow().clone()) else { return };
            let on = button.is_active();
            button.set_tooltip_text(Some(if on { "Unfreeze" } else { "Freeze in List" }));
            {
                let mut frozen = state.frozen_files.borrow_mut();
                frozen.retain(|p| *p != path);
                if on {
                    frozen.push(path.clone());
                }
            }
            state.persist_settings();
            // The row moves (pinned rows sit on top; an unpinned file that
            // is no longer recent leaves the list). Rebuild after this
            // handler, not under the button emitting it.
            glib::idle_add_local_once(move || state.update_files_panel());
        });
        button
    }

    /// Select the open document's row in the Files panel, or none.
    fn mark_current_file(&self) {
        let source = self.source.borrow();
        let index = source
            .container()
            .and_then(|current| self.files_order.borrow().iter().position(|p| p == current));
        match index.and_then(|i| self.files_list.row_at_index(i as i32)) {
            Some(row) => self.files_list.select_row(Some(&row)),
            None => self.files_list.unselect_all(),
        }
    }

    /// Re-stamp after writing the document ourselves.
    fn restamp(&self) {
        self.disk_stamp.set(self.source.borrow().text_file().and_then(|f| doc::disk_stamp(&f)));
        self.disk_banner.set_revealed(false);
    }

    /// Compare the open file with what we last read or wrote, and raise the
    /// banner when another program changed it. The stat runs on a worker
    /// thread (an unreachable share hangs it) and its answer is dropped if
    /// the document was replaced or re-saved meanwhile.
    fn check_disk_changes(self: &Rc<Self>) {
        let Some(file) = self.source.borrow().text_file() else { return };
        let Some(known) = self.disk_stamp.get() else { return };
        if self.disk_checking.replace(true) {
            return;
        }
        let seq = self.open_seq.get();
        let state = self.clone();
        glib::spawn_future_local(async move {
            let now = gio::spawn_blocking(move || doc::disk_stamp(&file)).await.ok().flatten();
            state.disk_checking.set(false);
            if state.open_seq.get() != seq || state.disk_stamp.get() != Some(known) {
                return;
            }
            match now {
                Some(now) if now == known => {}
                Some(_) => {
                    state.disk_banner.set_title("This file was changed by another program");
                    state.disk_banner.set_button_label(Some("Reload"));
                    state.disk_banner.set_revealed(true);
                }
                // Gone or moved; saving writes it back, so nothing to offer.
                None => {
                    state.disk_banner.set_title("This file was deleted or moved — saving recreates it");
                    state.disk_banner.set_button_label(None);
                    state.disk_banner.set_revealed(true);
                }
            }
        });
    }

    /// Sync the store's entry for the current document with the live
    /// collapsed set and view mode: MRU order, capped. No-op while the
    /// remember-folds toggle is off.
    fn sync_doc_state(&self) {
        if !self.remember_folds.get() {
            return;
        }
        let Some(path) = self.source.borrow().container().map(Path::to_path_buf) else {
            return;
        };
        let mut collapsed: Vec<String> = self.collapsed.borrow().iter().cloned().collect();
        collapsed.sort();
        let mut store = self.doc_state.borrow_mut();
        store.retain(|d| d.path != path);
        store.insert(0, DocState { path, collapsed, view: self.view_mode.get() });
        store.truncate(DOC_STATE_MAX);
    }

    /// Open a link destination with the system default application.
    fn open_link(self: &Rc<Self>, dest: &str) {
        if external_link(dest) {
            gtk4::UriLauncher::new(dest).launch(
                Some(&self.window),
                gio::Cancellable::NONE,
                |_| {},
            );
            return;
        }
        if dest.starts_with('#') {
            return;
        }
        let clean = bundle::decode_dest(dest);
        let source = self.source.borrow().clone();
        if matches!(source, DocSource::Pack { .. }) {
            return self.open_pack_attachment(&clean);
        }
        let raw = Path::new(&clean);
        let path = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            match source.preview_base() {
                Some(base) => base.join(raw),
                None => return self.toast("Save the document first to resolve relative links"),
            }
        };
        // No exists() check first: a stat on a dead share would hang the
        // window. The launch fails asynchronously instead, and says so.
        let state = self.clone();
        gtk4::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |result| {
                if let Err(err) = result {
                    if err.matches(gio::IOErrorEnum::NotFound) {
                        state.toast(&format!("Not found: {}", path.display()));
                    } else if !err.matches(gtk4::DialogError::Dismissed) {
                        state.toast(&format!("Could not open {}: {}", path.display(), err.message()));
                    }
                }
            },
        );
    }

    /// Open an attachment of a .textpack: extracted to a read-only temp copy
    /// (after a one-time explanation) and handed to the default application.
    /// Attachments staged from disk open their live original instead.
    fn open_pack_attachment(self: &Rc<Self>, rel: &str) {
        let Some(asset) = self.asset_source(rel) else {
            return self.toast(&format!("Not found in the pack: {rel}"));
        };
        if let bundle::AssetSource::File(path) = &asset {
            gtk4::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
                Some(&self.window),
                gio::Cancellable::NONE,
                |_| {},
            );
            return;
        }
        let bundle::AssetSource::Bytes(bytes) = asset else {
            return;
        };
        // Scope by a hash of the full pack path: two same-named packs must
        // not overwrite each other's extracted copies.
        let scope = self
            .source
            .borrow()
            .container()
            .map(|p| {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                p.hash(&mut hasher);
                format!("{:016x}", hasher.finish())
            })
            .unwrap_or_else(|| "pack".to_string());
        let rel = rel.to_string();
        let state = self.clone();
        self.explain_attachments_then(Box::new(move || {
            match bundle::extract_readonly_temp(&scope, &rel, &bytes) {
                Ok(path) => {
                    gtk4::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                        Some(&state.window),
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                }
                Err(err) => state.toast(&format!("Could not open the attachment: {err}")),
            }
        }));
    }

    /// One-time note about the read-only attachment model, then `then`.
    fn explain_attachments_then(self: &Rc<Self>, then: Box<dyn FnOnce()>) {
        if !self.show_attachment_note.get() {
            return then();
        }
        let dialog = adw::AlertDialog::new(
            Some("Attachments Open as Read-Only Copies"),
            Some(
                "Files inside a .textpack open as read-only copies. \
                 To edit one: right-click its link, save it somewhere, \
                 edit that copy, then attach it again.",
            ),
        );
        dialog.add_responses(&[("never", "Don't Show _Again"), ("ok", "_OK")]);
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("ok");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response.as_str() == "never" {
                state.show_attachment_note.set(false);
                state.persist_settings();
            }
            then();
        });
    }

    /// Right-click on a preview link: save a copy of the referenced file.
    fn save_attachment_dialog(self: &Rc<Self>, dest: &str) {
        if external_link(dest) || dest.starts_with('#') {
            return;
        }
        let clean = bundle::decode_dest(dest);
        let Some(asset) = self.asset_source(&clean) else {
            return self.toast(&format!("Not found: {clean}"));
        };
        let name = Path::new(&clean)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".to_string());
        let dialog = gtk4::FileDialog::builder()
            .title("Save Attachment")
            .initial_name(name)
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                let written = match &asset {
                    bundle::AssetSource::Bytes(bytes) => doc::write_atomic(&path, bytes),
                    bundle::AssetSource::File(src) => copy_atomic(src, &path),
                };
                match written {
                    Ok(()) => state.toast(&format!(
                        "Saved {}",
                        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                    )),
                    Err(err) => state.toast(&format!("Could not save: {err}")),
                }
            }
        });
    }

    /// Click in the preview: move the editor cursor to the matching source
    /// position; a selection (e.g. a double-clicked word) selects the
    /// matching source range. Runs from an idle so the preview's own click
    /// handling (word selection) has already happened.
    fn sync_editor_to_preview(&self, x: f64, y: f64) {
        let map = self.preview_map.borrow();
        let max = self.buffer.char_count();
        if let Some((start, end)) = self.preview.buffer().selection_bounds() {
            let bounds = (
                map.source_offset(start.offset()),
                map.source_offset(end.offset()),
            );
            if let (Some(a), Some(b)) = bounds {
                if a < b && b <= max {
                    let end = self.buffer.iter_at_offset(b);
                    let start = self.buffer.iter_at_offset(a);
                    self.syncing_cursor.set(true);
                    self.buffer.select_range(&start, &end);
                    self.syncing_cursor.set(false);
                    // Mirror the selection but keep focus on the preview, so
                    // Ctrl+C copies the word just selected there.
                    self.editor
                        .scroll_to_mark(&self.buffer.get_insert(), 0.0, true, 0.0, 0.5);
                    return;
                }
            }
        }
        let (bx, by) = self
            .preview
            .window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
        let Some(iter) = self.preview.iter_at_location(bx, by) else {
            return;
        };
        let Some(source) = map.source_offset(iter.offset()) else {
            return;
        };
        let iter = self.buffer.iter_at_offset(source.min(max));
        self.syncing_cursor.set(true);
        self.buffer.place_cursor(&iter);
        self.syncing_cursor.set(false);
        self.center_editor_on_cursor();
    }

    /// Center the editor vertically on its cursor and focus it.
    fn center_editor_on_cursor(&self) {
        self.editor
            .scroll_to_mark(&self.buffer.get_insert(), 0.0, true, 0.0, 0.5);
        self.editor.grab_focus();
    }

    // --- View mode ---

    /// Apply the two panel toggles: the Insert list, the preview, and the
    /// sidebar itself, which goes away once both are hidden so the main view
    /// gets the whole window. In view mode the bottom slot holds the editor,
    /// which the preview toggle has no business hiding.
    fn apply_panels(self: &Rc<Self>) {
        self.apply_panels_then_render(true);
    }

    /// `render` is false only on document load, where the caller renders once
    /// itself straight afterwards — and from the top rather than holding the
    /// previous document's scroll position.
    fn apply_panels_then_render(self: &Rc<Self>, render: bool) {
        // Non-markdown documents override both toggles off without changing
        // them, so the next markdown file gets the layout back.
        let usable = self.doc_kind().uses_panels();
        let insert = self.show_insert.get() && usable;
        let bottom = self.show_preview.get() && usable;
        // Files is navigation, not a view of the document, so it stays for
        // every kind.
        let files = self.show_files.get();
        self.files_panel.set_visible(files);
        self.top_panel.set_visible(insert);
        self.bottom_panel.set_visible(bottom);
        self.lower.set_visible(insert || bottom);
        self.sidebar.set_visible(files || insert || bottom);
        // Editing skips the render while the preview is hidden, so what is
        // in its buffer is stale by the time it comes back.
        if render && self.preview_visible() {
            self.render_preview(true);
        }
    }

    /// Whether the preview is on screen at all: it sits in the sidebar's
    /// bottom slot unless toggled off, and in view mode it is the main view —
    /// which is the one place a hidden panel still shows the preview.
    fn preview_visible(&self) -> bool {
        (self.show_preview.get() && self.doc_kind().uses_panels()) || self.view_mode.get()
    }

    /// Set view mode programmatically, keeping the win.view-mode action state
    /// (and the bound header toggle) in sync.
    /// The view mode opening `path` settles on: the remembered one, else the
    /// open-in-view setting; view mode is never dropped on open.
    fn view_mode_for(&self, path: &Path) -> bool {
        let remembered = self
            .remember_folds
            .get()
            .then(|| self.doc_state.borrow().iter().find(|d| d.path == path).map(|d| d.view))
            .flatten();
        remembered.unwrap_or_else(|| self.view_mode.get() || self.open_in_view.get())
    }

    /// Settle the layout for the document a fresh window is about to open,
    /// before it is shown. Entering view mode moves the editor, and moving
    /// it in the first moments after it takes focus crashes GTK 4.22's
    /// Wayland input method: until the compositor's text-input object is
    /// bound, focus-out leaves `global->current` pointing at the editor's IM
    /// context, which the move frees, and the next text-input event
    /// dereferences it. The open completion then finds nothing to change.
    fn preset_view_mode(self: &Rc<Self>, path: &Path) {
        self.set_view_mode(self.view_mode_for(path) && DocKind::of(path).uses_panels());
    }

    /// Focus whatever fills the main slot (the editor whenever it is shown).
    fn focus_main(&self) {
        if self.view_mode.get() && !self.show_preview.get() {
            self.preview.grab_focus();
        } else {
            self.editor.grab_focus();
        }
    }

    fn set_view_mode(self: &Rc<Self>, view: bool) {
        // There is nothing to switch *to* without a rendered preview. Caught
        // here rather than at the call sites because three of them ask for
        // view mode from remembered or global state, not from a click.
        let view = view && self.doc_kind().uses_panels();
        if self.view_mode.get() == view {
            return;
        }
        if let Some(action) = self.window.lookup_action("view-mode") {
            if let Some(action) = action.downcast_ref::<gio::SimpleAction>() {
                action.set_state(&view.to_variant());
            }
        }
        self.apply_view_mode(view);
    }

    /// Put `main` into the pane holding the document's main widget: hpaned's
    /// end child in the default layout, its start child in horizontal layout.
    fn set_main_widget(&self, main: Option<&gtk4::ScrolledWindow>) {
        if self.horizontal.get() {
            self.hpaned.set_start_child(main);
        } else {
            self.hpaned.set_end_child(main);
        }
    }

    /// Swap the preview and the editor between the main pane and the
    /// sidebar's bottom slot. The paned positions are untouched.
    fn apply_view_mode(self: &Rc<Self>, view: bool) {
        if self.view_mode.replace(view) == view {
            return;
        }
        self.set_main_widget(None);
        if view {
            self.bottom_panel.remove(&self.preview_scroll);
            self.editor_scroll.set_size_request(-1, -1);
            self.bottom_panel.append(&self.editor_scroll);
            self.preview_scroll.set_size_request(320, -1);
            self.set_main_widget(Some(&self.preview_scroll));
            self.bottom_label.set_text("Editor");
        } else {
            self.bottom_panel.remove(&self.editor_scroll);
            self.preview_scroll.set_size_request(-1, -1);
            self.bottom_panel.append(&self.preview_scroll);
            self.editor_scroll.set_size_request(320, -1);
            self.set_main_widget(Some(&self.editor_scroll));
            self.bottom_label.set_text("Preview");
        }
        // No side panel: the toggle just swaps what fills the window, and the
        // other view is hidden along with the slot it moved into. The preview
        // stopped rendering while it was out of sight, so it comes back to
        // the main slot stale.
        if view && !self.show_preview.get() {
            self.render_preview(false);
        }
        self.focus_main();
    }

    /// Rearrange the paneds: the default layout is the sidebar (files over
    /// insert over preview) left, main pane right; horizontal layout is main
    /// pane on top with the sidebar's panels side by side below.
    fn apply_horizontal(&self, horizontal: bool) {
        if self.horizontal.get() == horizontal {
            return;
        }
        let main = if self.view_mode.get() {
            self.preview_scroll.clone()
        } else {
            self.editor_scroll.clone()
        };
        self.hpaned.set_start_child(gtk4::Widget::NONE);
        self.hpaned.set_end_child(gtk4::Widget::NONE);
        self.horizontal.set(horizontal);
        if horizontal {
            self.hpaned.set_orientation(gtk4::Orientation::Vertical);
            self.sidebar.set_orientation(gtk4::Orientation::Horizontal);
            self.lower.set_orientation(gtk4::Orientation::Horizontal);
            self.hpaned.set_start_child(Some(&main));
            self.hpaned.set_end_child(Some(&self.sidebar));
            self.hpaned.set_resize_start_child(true);
            self.hpaned.set_resize_end_child(false);
            self.sidebar.set_size_request(-1, 150);
        } else {
            self.hpaned.set_orientation(gtk4::Orientation::Horizontal);
            self.sidebar.set_orientation(gtk4::Orientation::Vertical);
            self.lower.set_orientation(gtk4::Orientation::Vertical);
            self.hpaned.set_start_child(Some(&self.sidebar));
            self.hpaned.set_end_child(Some(&main));
            self.hpaned.set_resize_start_child(false);
            self.hpaned.set_resize_end_child(true);
            self.sidebar.set_size_request(200, -1);
        }
    }

    // --- Document state ---

    fn load_text(self: &Rc<Self>, text: &str, source: DocSource) {
        self.loading.set(true);
        // Toggling undo off clears the history of the previous document.
        self.buffer.set_enable_undo(false);
        self.buffer.set_text(text);
        self.buffer.set_enable_undo(true);
        // set_text leaves the right-gravity insert mark at the buffer end;
        // start new documents at the top.
        self.buffer.place_cursor(&self.buffer.start_iter());
        self.loading.set(false);
        *self.source.borrow_mut() = source;
        self.staged_assets.borrow_mut().clear();
        self.pack_bytes_cache.borrow_mut().clear();
        preview::reset_loader_cache();
        {
            // Restore this document's remembered folds; unknown keys are
            // pruned by the next render.
            let mut collapsed = self.collapsed.borrow_mut();
            collapsed.clear();
            if let Some(path) = self.source.borrow().container().filter(|_| self.remember_folds.get())
            {
                if let Some(doc) = self.doc_state.borrow().iter().find(|d| d.path == path) {
                    collapsed.extend(doc.collapsed.iter().cloned());
                }
            }
        }
        self.doc_allows_remote.set(false);
        // The caller stamps a document read from disk; anything else has no
        // file to watch.
        self.disk_stamp.set(None);
        self.disk_banner.set_revealed(false);
        self.modified.set(false);
        // A checked-out memo carries its unpushed state across sessions.
        self.memo_unpushed.set(self.memo_doc().is_some_and(|doc| doc.unpushed));
        self.update_memos_actions();
        self.update_title();
        self.apply_kind_layout(false);
        // Same reason `schedule_preview` skips: rendering a preview nobody
        // can see is pure cost, and whatever reveals it renders first.
        if self.preview_visible() {
            self.render_preview(false);
        } else {
            self.forget_preview_sections();
        }
        // The buffer holds a different document now; nothing about the old
        // one's highlighting carries over.
        self.reset_highlight();
        self.apply_highlight();
        self.editor.grab_focus();
    }

    /// The document kind decides the layout: markdown gets the panels and
    /// wrapped lines, plain kinds get the editor alone, and NFO/DIZ art
    /// additionally needs its exact line widths (the preview used to be the
    /// only unwrapped view of it; now the editor is). `render` as in
    /// `apply_panels_then_render`.
    fn apply_kind_layout(self: &Rc<Self>, render: bool) {
        let kind = self.doc_kind();
        self.apply_wrap();
        self.set_view_mode(self.view_mode.get() && kind.uses_panels());
        self.update_kind_actions();
        self.apply_panels_then_render(render);
    }

    /// Only a render refreshes the section list; without one the previous
    /// document's sections would keep the fold button live over a preview
    /// that is not on screen.
    fn forget_preview_sections(&self) {
        self.preview_sections.borrow_mut().clear();
        self.update_fold_btn();
    }

    /// Save As gave the same text a different kind (Untitled → notes.yaml,
    /// notes.txt → notes.md): re-apply everything `load_text` derives from
    /// the kind, instead of keeping the old layout until the file is reopened.
    fn kind_changed(self: &Rc<Self>) {
        self.apply_kind_layout(true);
        if !self.preview_visible() {
            self.forget_preview_sections();
        }
        self.reset_highlight();
        self.apply_highlight();
    }

    fn new_file(self: &Rc<Self>) {
        let state = self.clone();
        self.guard_unsaved(Box::new(move || {
            // Invalidate any in-flight async open so a slow read from a
            // hung mount can't replace the fresh document later.
            state.open_seq.set(state.open_seq.get().wrapping_add(1));
            state.load_text("", DocSource::None);
            state.set_view_mode(false);
        }));
    }

    fn open_path(self: &Rc<Self>, path: &Path) {
        self.open_path_inner(path, OpenTry::First);
    }

    /// Only the first try mounts the gvfs share a missing path lives on; the
    /// retry after mounting doesn't, so a share that comes up without the
    /// file reports the error instead of looping.
    fn open_path_inner(self: &Rc<Self>, path: &Path, attempt: OpenTry) {
        // Classify and read on a worker thread: any stat or read can block
        // for minutes on an unreachable FUSE/network mount, which would
        // freeze the main loop. A stale completion (a newer open started
        // meanwhile) is dropped via open_seq.
        let path = path.to_path_buf();
        let seq = self.open_seq.get().wrapping_add(1);
        self.open_seq.set(seq);
        let state = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let loaded = gio::spawn_blocking(move || {
                if attempt == OpenTry::AfterMount {
                    doc::wait_for_path(&p);
                }
                let read = if bundle::is_bundle(&p) {
                    bundle::open_bundle(&p).map(|(text, text_name, legacy)| {
                        (text, DocSource::Bundle { dir: p.clone(), text_name }, legacy)
                    })
                } else if bundle::is_pack(&p) {
                    bundle::open_pack(&p)
                        .map(|(text, info)| (text, DocSource::Pack { path: p.clone(), info }, None))
                } else if odf::is_odt(&p) {
                    odf::import(&p).map(|text| (text, DocSource::Odt { path: p.clone() }, None))
                } else {
                    doc::read_text(&p)
                        .map(|(text, legacy)| (text, DocSource::Plain(p.clone()), legacy))
                }
                .map(|(text, source, legacy)| {
                    let stamp = source.text_file().and_then(|f| doc::disk_stamp(&f));
                    (text, source, legacy, stamp)
                });
                // Not found in a folder that is still there: deleted or
                // moved for good. A missing folder is more likely a share or
                // drive that is not attached right now, so that is no proof.
                let gone = matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
                    && p.parent().is_some_and(Path::is_dir);
                (read, gone)
            })
            .await;
            if state.open_seq.get() != seq {
                return;
            }
            match loaded {
                Ok((Ok((text, source, legacy, stamp)), _)) => {
                    let imported = matches!(source, DocSource::Odt { .. });
                    state.load_text(&text, source);
                    state.disk_stamp.set(stamp);
                    if imported {
                        state.toast("Imported as Markdown — saving creates a new file");
                    }
                    if let Some(encoding) = legacy {
                        state.toast(&format!(
                            "Not UTF-8 — read as {encoding}; saving converts it to UTF-8"
                        ));
                    }
                    // Remembered view mode wins; without a memory the
                    // open-in-view setting applies, and view mode is never
                    // dropped on open.
                    state.set_view_mode(state.view_mode_for(&path));
                    // Memo cache files are reached through the browse
                    // dialog; keeping them out of "Open Recent" stops it
                    // filling with ~/.cache paths.
                    if state.memo_doc_for(&path).is_none() {
                        state.remember_recent(&path);
                        state.update_recent_menu();
                    }
                    state.persist_settings();
                }
                Ok((Err(_), true))
                    if state.recent_files.borrow().contains(&path)
                        || state.frozen_files.borrow().contains(&path) =>
                {
                    state.recent_files.borrow_mut().retain(|p| *p != path);
                    state.frozen_files.borrow_mut().retain(|p| *p != path);
                    state.update_recent_menu();
                    state.persist_settings();
                    state.toast(&format!(
                        "{} no longer exists — removed from the list",
                        file_label(&path)
                    ));
                }
                Ok((Err(err), _)) => {
                    // A remembered file on a share that is simply not mounted
                    // right now: bring the share up and open it after all.
                    if attempt == OpenTry::First
                        && err.kind() == std::io::ErrorKind::NotFound
                        && state.mount_share(&path)
                    {
                        return;
                    }
                    state.toast(&format!("Could not open {}: {err}", path.display()));
                }
                Err(_) => state.toast(&format!("Could not open {}", path.display())),
            }
        });
    }

    /// Mount the gvfs share a missing path lives on, then open it again.
    /// GTK's `MountOperation` supplies the password prompt, so this is the
    /// same thing that happens when the share's bookmark is clicked in
    /// Files — usually silent, since gvfs keeps the password in the keyring.
    /// Returns false when the path is not on a gvfs share, leaving the
    /// caller to report its error as before.
    fn mount_share(self: &Rc<Self>, path: &Path) -> bool {
        let Some(uri) = doc::gvfs_share_uri(path) else {
            return false;
        };
        let op = gtk4::MountOperation::new(Some(&self.window));
        let state = self.clone();
        let path = path.to_path_buf();
        let name = uri.clone();
        self.toast(&format!("Connecting to {name}…"));
        gio::File::for_uri(&uri).mount_enclosing_volume(
            gio::MountMountFlags::NONE,
            Some(&op),
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(()) => state.open_path_inner(&path, OpenTry::AfterMount),
                Err(err) if err.matches(gio::IOErrorEnum::AlreadyMounted) => {
                    state.open_path_inner(&path, OpenTry::AfterMount)
                }
                // Dismissing the password dialog is an answer, not an error;
                // FailedHandled means GTK has already shown one.
                Err(err)
                    if err.matches(gio::IOErrorEnum::Cancelled)
                        || err.matches(gio::IOErrorEnum::FailedHandled) => {}
                Err(err) => state.toast(&format!("Could not connect to {name}: {}", err.message())),
            },
        );
        true
    }

    /// The local path of a file picked in a dialog. Closing the dialog is
    /// silent; anything else says why nothing happened. A picked file has no
    /// path when it lives on a location GIO only knows by URI — a gvfs share
    /// (smb://, sftp://, dav://…) whose FUSE mount the process can't see,
    /// which inside Flatpak needs `--filesystem=xdg-run/gvfs`. Bindle is
    /// path-based throughout, so those can't be opened or written.
    fn picked_path(&self, result: Result<gio::File, glib::Error>) -> Option<PathBuf> {
        let file = match result {
            Ok(file) => file,
            Err(err) => {
                if !err.matches(gtk4::DialogError::Dismissed)
                    && !err.matches(gtk4::DialogError::Cancelled)
                {
                    self.toast(&format!("File dialog failed: {}", err.message()));
                }
                return None;
            }
        };
        match file.path() {
            Some(path) => Some(path),
            None => {
                self.toast(&format!("Cannot use {} — mount the share locally first", file.uri()));
                None
            }
        }
    }

    fn open_dialog(self: &Rc<Self>) {
        let state = self.clone();
        self.guard_unsaved(Box::new(move || {
            let dialog = gtk4::FileDialog::builder()
                .title("Open Markdown File")
                .filters(&md_filters())
                .build();
            let s = state.clone();
            dialog.open(Some(&state.window), gio::Cancellable::NONE, move |result| {
                if let Some(path) = s.picked_path(result) {
                    s.open_path(&path);
                }
            });
        }));
    }

    /// A .textbundle is a directory, which the file-mode Open dialog can
    /// only navigate into — this uses a folder picker instead.
    fn open_bundle_dialog(self: &Rc<Self>) {
        let state = self.clone();
        self.guard_unsaved(Box::new(move || {
            let dialog = gtk4::FileDialog::builder().title("Open TextBundle").build();
            let s = state.clone();
            dialog.select_folder(Some(&state.window), gio::Cancellable::NONE, move |result| {
                if let Some(path) = s.picked_path(result) {
                    if bundle::is_bundle(&path) {
                        s.open_path(&path);
                    } else {
                        s.toast("That folder is not a .textbundle");
                    }
                }
            });
        }));
    }

    /// Save, first asking before replacing changes another program made
    /// since we read the file. The stat is synchronous like the write that
    /// follows it; a deleted file just gets written back.
    fn save(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        let changed = {
            let source = self.source.borrow();
            let writes_back =
                matches!(*source, DocSource::Plain(_) | DocSource::Bundle { .. } | DocSource::Pack { .. });
            match (writes_back, self.disk_stamp.get(), source.text_file()) {
                (true, Some(known), Some(file)) => doc::disk_stamp(&file).is_some_and(|now| now != known),
                _ => false,
            }
        };
        if !changed {
            return self.save_now(then);
        }
        let dialog = adw::AlertDialog::new(
            Some("File Changed on Disk"),
            Some("Another program changed this file after Bindle opened it. Saving replaces those changes with yours."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("overwrite", "_Overwrite")]);
        dialog.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response == "overwrite" {
                state.save_now(then);
            }
        });
    }

    fn save_now(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        let source = self.source.borrow().clone();
        match source {
            // Markdown written over the .odt would destroy everything the
            // import dropped, so an imported document always saves anew.
            DocSource::None | DocSource::Odt { .. } => self.save_as(then),
            DocSource::Plain(path) => self.write_to(&path, then),
            DocSource::Bundle { dir, text_name } => {
                match doc::write_atomic(&dir.join(&text_name), self.text().as_bytes()) {
                    Ok(()) => self.finish_save(&dir, then),
                    Err(err) => self.toast(&format!("Could not save: {err}")),
                }
            }
            DocSource::Pack { path, info } => {
                let staged = self.staged_assets.borrow().clone();
                match bundle::save_pack(&path, &info, self.text().as_str(), &staged) {
                    Ok(entries) => {
                        // What later staging must not collide with.
                        if let DocSource::Pack { info, .. } = &mut *self.source.borrow_mut() {
                            info.entries = entries;
                        }
                        // The zip content changed; cached entry bytes are stale.
                        self.staged_assets.borrow_mut().clear();
                        self.pack_bytes_cache.borrow_mut().clear();
                        self.finish_save(&path, then);
                    }
                    Err(err) => self.toast(&format!("Could not save: {err}")),
                }
            }
        }
    }

    fn text(&self) -> glib::GString {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
    }

    fn finish_save(&self, shown: &Path, then: Option<Box<dyn FnOnce()>>) {
        self.restamp();
        self.modified.set(false);
        self.update_title();
        let name = shown.file_name().map(|n| n.to_string_lossy().to_string());
        self.toast(&format!("Saved {}", name.unwrap_or_default()));
        if let Some(then) = then {
            then();
        }
    }

    fn save_as(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        // A plain .md cannot contain referenced images/attachments — offer
        // TextPack when the document has any.
        if !self.has_local_refs() {
            return self.save_as_markdown(then);
        }
        // An unsaved document's only local assets are pasted images, and
        // Markdown Only does keep those (see `write_pasted_images`).
        let body = if matches!(*self.source.borrow(), DocSource::None)
            && bundle::local_asset_dests(&self.text())
                .iter()
                .all(|d| self.staged_assets.borrow().contains_key(&bundle::decode_dest(d)))
        {
            "Pasted images are saved into an assets folder next to a markdown file; \
             a TextPack keeps everything in one file."
        } else {
            "This document references local images or attachments. \
             A plain markdown file will not contain them; \
             a TextPack keeps everything in one file."
        };
        let dialog = adw::AlertDialog::new(Some("Keep Attached Files?"), Some(body));
        dialog.add_responses(&[
            ("cancel", "_Cancel"),
            ("md", "_Markdown Only"),
            ("pack", "Save as Text_Pack"),
        ]);
        dialog.set_response_appearance("pack", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("pack"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            match response.as_str() {
                "pack" => state.pack_save_dialog(true, then),
                "md" => state.save_as_markdown(then),
                _ => {}
            }
        });
    }

    /// Anything the document references that a plain .md file would lose.
    /// Non-markdown documents have no markdown asset references, even when
    /// their text happens to contain link-shaped strings.
    fn has_local_refs(&self) -> bool {
        if !self.doc_kind().is_markdown() {
            return false;
        }
        !self.staged_assets.borrow().is_empty()
            || !bundle::local_asset_dests(&self.text()).is_empty()
    }

    fn save_as_markdown(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        // An imported .odt suggests its own name; other kinds keep the plain
        // default rather than turning notes.yaml into notes.md.
        let initial = match &*self.source.borrow() {
            DocSource::Odt { .. } => self.export_doc_name("md"),
            _ => "Untitled.md".to_string(),
        };
        let dialog = gtk4::FileDialog::builder()
            .title("Save Markdown File")
            .initial_name(initial)
            .filters(&md_filters())
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                state.write_to(&path, then);
            }
        });
    }

    /// Write the buffer to a plain markdown file (Save As always saves plain).
    fn write_to(self: &Rc<Self>, path: &Path, then: Option<Box<dyn FnOnce()>>) {
        let (text, written) = match self.write_pasted_images(path) {
            Ok(done) => done,
            Err(err) => return self.toast(&format!("Could not save the pasted images: {err}")),
        };
        match doc::write_atomic(path, text.as_bytes()) {
            Ok(()) => {
                if text != self.text().as_str() {
                    self.replace_text(&text);
                }
                let old_kind = self.doc_kind();
                let moved = self.source.borrow().container() != Some(path);
                *self.source.borrow_mut() = DocSource::Plain(path.to_path_buf());
                if moved {
                    // A checked-out memo saved under a new name is an
                    // ordinary file now: nothing of it to push, and "Push to
                    // Memos" must not stay offered for it. (The memo's own
                    // entry keeps its flag — it was stored when it flipped.)
                    self.memo_unpushed.set(self.memo_doc().is_some_and(|d| d.unpushed));
                    self.update_memos_actions();
                    // Into the Files panel, like a file opened by hand.
                    if self.memo_doc_for(path).is_none() {
                        self.remember_recent(path);
                        self.update_recent_menu();
                    }
                }
                // No longer a pack: staged entries must not leak into a
                // later export via asset_source's staged-first lookup.
                self.staged_assets.borrow_mut().clear();
                self.pack_bytes_cache.borrow_mut().clear();
                if self.doc_kind() != old_kind {
                    self.kind_changed();
                }
                self.finish_save(path, then);
            }
            Err(err) => {
                // Nothing was saved: take back the images written for it, or
                // every retry would leave another numbered copy behind.
                for file in written {
                    let _ = fs::remove_file(file);
                }
                self.toast(&format!("Could not save: {err}"));
            }
        }
    }

    /// Replace the whole buffer with `text` as one undoable edit, keeping
    /// the cursor's offset.
    fn replace_text(&self, text: &str) {
        let offset = self.buffer.iter_at_mark(&self.buffer.get_insert()).offset();
        self.buffer.begin_user_action();
        let (mut start, mut end) = self.buffer.bounds();
        self.buffer.delete(&mut start, &mut end);
        self.buffer.insert(&mut self.buffer.start_iter(), text);
        self.buffer.end_user_action();
        self.buffer.place_cursor(&self.buffer.iter_at_offset(offset));
    }

    /// Before saving as plain markdown at `path`: write images that exist
    /// only in memory (pasted into an unsaved document or a pack) into the
    /// `assets/` folder beside it, where their `assets/…` references point.
    /// Returns the text to save — with references renumbered where a name
    /// was already taken there — and the files written. The buffer is left
    /// alone: `write_to` adopts the text only once the save succeeded.
    fn write_pasted_images(&self, path: &Path) -> std::io::Result<(String, Vec<PathBuf>)> {
        let pasted: Vec<(String, Vec<u8>)> = self
            .staged_assets
            .borrow()
            .iter()
            .filter_map(|(key, source)| match source {
                bundle::AssetSource::Bytes(bytes) => Some((key.clone(), bytes.clone())),
                bundle::AssetSource::File(_) => None,
            })
            .collect();
        let text = self.text().to_string();
        if pasted.is_empty() {
            return Ok((text, Vec::new()));
        }
        let referenced: HashSet<String> =
            bundle::local_asset_dests(&text).iter().map(|d| bundle::decode_dest(d)).collect();
        let assets = path.parent().unwrap_or(Path::new(".")).join("assets");
        let mut renames = Vec::new();
        let mut written = Vec::new();
        for (key, bytes) in pasted {
            // A paste since deleted (or undone) leaves nothing behind.
            if !referenced.contains(&key) {
                continue;
            }
            let name = key.strip_prefix("assets/").unwrap_or(&key);
            let stored = match bundle::write_into_assets(name, &bytes, &assets) {
                Ok(stored) => stored,
                Err(err) => {
                    for file in written {
                        let _ = fs::remove_file(file);
                    }
                    return Err(err);
                }
            };
            written.push(assets.join(&stored));
            if stored != name {
                renames.push((key.clone(), stored));
            }
        }
        Ok((bundle::rewrite_asset_dests(&text, &renames), written))
    }

    // --- TextPack export ---

    fn export_pack_dialog(self: &Rc<Self>) {
        // A TextPack is a markdown container; wrapping YAML/JSON/… in one
        // only produces a file that reopens as mangled markdown.
        if !self.doc_kind().is_markdown() {
            return self.toast("Only markdown documents can be exported as a TextPack");
        }
        self.pack_save_dialog(false, None);
    }

    /// Ask for a .textpack destination and write the document there.
    /// `rebind` gives Save As semantics: the window switches to editing the
    /// new pack; false is a plain export that leaves the document alone.
    ///
    /// Files referenced from outside the document's folder are listed for
    /// confirmation first: a document written by someone else can point at
    /// any file this user can read (`~/.ssh` included), and the pack would
    /// carry a copy of it to whoever it is sent to.
    fn pack_save_dialog(self: &Rc<Self>, rebind: bool, then: Option<Box<dyn FnOnce()>>) {
        let outside = self.outside_refs();
        if outside.is_empty() {
            return self.pick_pack_dest(rebind, then);
        }
        let mut list: Vec<String> = outside.iter().take(8).map(|p| format!("• {p}")).collect();
        if outside.len() > 8 {
            list.push(format!("…and {} more", outside.len() - 8));
        }
        let dialog = adw::AlertDialog::new(
            Some("Include Files From Elsewhere?"),
            Some(&format!(
                "The TextPack will contain copies of these files from outside the document's folder:\n\n{}",
                list.join("\n")
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("include", "_Include Them")]);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response == "include" {
                state.pick_pack_dest(rebind, then);
            }
        });
    }

    /// Local files the document references from outside its own folder —
    /// absolute paths, or relative ones climbing out with `..` — that a
    /// TextPack export would copy in. Archive entries can't leave their
    /// archive, and staged files were picked by the user, so neither counts.
    fn outside_refs(&self) -> Vec<String> {
        if self.source.borrow().archive().is_some() {
            return Vec::new();
        }
        bundle::local_asset_dests(&self.text())
            .iter()
            .map(|dest| bundle::decode_dest(dest))
            .filter(|clean| !self.staged_assets.borrow().contains_key(clean))
            .filter(|clean| escapes_folder(Path::new(clean)))
            .filter(|clean| self.asset_source(clean).is_some())
            .collect()
    }

    fn pick_pack_dest(self: &Rc<Self>, rebind: bool, then: Option<Box<dyn FnOnce()>>) {
        let name = self
            .source
            .borrow()
            .container()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "Untitled".to_string());
        let dialog = gtk4::FileDialog::builder()
            .title(if rebind { "Save as TextPack" } else { "Export as TextPack" })
            .initial_name(format!("{name}.textpack"))
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                if state.export_pack_to(&path) && rebind {
                    state.open_path(&path);
                    if let Some(then) = then {
                        then();
                    }
                }
            }
        });
    }

    /// Bundle the document and every referenced local image and linked file
    /// into `dest`, rewriting their references to `assets/<name>`.
    /// Returns whether the pack was written.
    fn export_pack_to(self: &Rc<Self>, dest: &Path) -> bool {
        let text = self.text().to_string();
        let mut assets: Vec<(String, bundle::AssetSource)> = Vec::new();
        let mut renames: Vec<(String, String)> = Vec::new();
        let mut skipped = 0;
        for dest_ref in bundle::local_asset_dests(&text) {
            let clean = bundle::decode_dest(&dest_ref);
            let Some(source) = self.asset_source(&clean) else {
                skipped += 1;
                continue;
            };
            let base_name = Path::new(&clean)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "image".to_string());
            let name = bundle::unique_name(&base_name, |candidate| {
                assets.iter().any(|(existing, _)| existing == candidate)
            });
            assets.push((name.clone(), source));
            renames.push((dest_ref, name));
        }
        let rewritten = bundle::rewrite_asset_dests(&text, &renames);
        match bundle::export_pack(dest, &rewritten, &assets) {
            Ok(()) => {
                let name = dest
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if skipped == 0 {
                    self.toast(&format!("Exported {name}"));
                } else {
                    self.toast(&format!(
                        "Exported {name} — {skipped} referenced file(s) not found"
                    ));
                }
                true
            }
            Err(err) => {
                self.toast(&format!("Could not export: {err}"));
                false
            }
        }
    }

    /// Content of a local image/file reference of the current document, or
    /// None when it doesn't resolve to a readable file. No size cap: these
    /// bytes are copied, not decoded.
    fn asset_source(&self, rel: &str) -> Option<bundle::AssetSource> {
        if let Some(source) = self.staged_assets.borrow().get(rel) {
            return Some(source.clone());
        }
        let source = self.source.borrow().clone();
        if let Some((path, info)) = source.archive() {
            return bundle::read_asset(&path, &info, rel, bundle::COPY_MAX_BYTES)
                .map(bundle::AssetSource::Bytes);
        }
        let raw = Path::new(rel);
        let path = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            source.preview_base()?.join(raw)
        };
        path.is_file().then(|| bundle::AssetSource::File(path))
    }

    /// Format of the current document: markdown unless a plain file's
    /// extension says otherwise (bundles and packs are always markdown).
    fn doc_kind(&self) -> DocKind {
        match &*self.source.borrow() {
            DocSource::Plain(path) => DocKind::of(path),
            _ => DocKind::Markdown,
        }
    }

    /// Attach or detach the line-number gutter on the editor's left window.
    fn apply_line_numbers(&self, on: bool) {
        if on {
            self.editor.set_gutter(gtk4::TextWindowType::Left, Some(&self.gutter));
            self.update_gutter();
        } else {
            self.editor.set_gutter(gtk4::TextWindowType::Left, None::<&gtk4::Widget>);
        }
    }

    /// Select the whole line under gutter y-coordinate `y`, trailing newline
    /// included so cut/delete removes the line. With `extend`, the existing
    /// selection — or, with none, the cursor's line — grows to cover it
    /// instead (a text buffer holds one contiguous selection, so picking
    /// lines 3 and 7 selects 3–7).
    fn select_gutter_line(&self, y: f64, extend: bool) {
        let (_, by) = self.editor.window_to_buffer_coords(gtk4::TextWindowType::Left, 0, y as i32);
        let (start, _) = self.editor.line_at_y(by);
        self.pick_source_lines(start.line(), start.line(), extend);
        self.editor.grab_focus();
    }

    /// Select source lines `first..=last` whole and mirror them in the
    /// preview — the shared half of a gutter pick and a pick made beside a
    /// line in the preview. `extend` behaves as in `select_gutter_line`.
    fn pick_source_lines(&self, first: i32, last: i32, extend: bool) {
        let Some(mut start) = self.buffer.iter_at_line(first) else {
            return;
        };
        let mut end = self.buffer.iter_at_line(last).unwrap_or_else(|| self.buffer.end_iter());
        if !end.forward_line() {
            end.forward_to_end();
        }
        if extend {
            let (mut s, mut e) = self.buffer.selection_bounds().unwrap_or_else(|| {
                // A bare cursor anchors its whole line, even at a line start.
                let cursor = self.buffer.iter_at_mark(&self.buffer.get_insert());
                let mut e = cursor;
                e.set_line_offset(0);
                if !e.forward_line() {
                    e.forward_to_end();
                }
                (cursor, e)
            });
            s.set_line_offset(0);
            if !e.starts_line() && !e.forward_line() {
                e.forward_to_end();
            }
            if s.offset() < start.offset() {
                start = s;
            }
            if e.offset() > end.offset() {
                end = e;
            }
        }
        self.buffer.select_range(&end, &start);
        self.line_pick.set(Some((start.offset(), end.offset())));
        self.apply_line_pick();
    }

    /// The source lines behind the preview line at widget point (`x`, `y`),
    /// when the point lies beside that line's text — in the left margin or
    /// past its end — rather than on it. A preview line is a whole paragraph,
    /// list item or table row, so it may stand for several source lines.
    fn preview_line_beside(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        let (bx, by) = self
            .preview
            .window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
        let iter = self.preview.iter_at_location(bx, by)?;
        // The point is on text when it falls inside the character nearest to
        // it; a line end has no width, hence the floor of one pixel.
        let rect = self.preview.iter_location(&iter);
        if bx >= rect.x() && bx < rect.x() + rect.width().max(1) {
            return None;
        }
        let mut line_start = iter;
        line_start.set_line_offset(0);
        let mut line_end = iter;
        if !line_end.ends_line() {
            line_end.forward_to_line_end();
        }
        self.preview_source_lines(line_start.offset(), line_end.offset())
    }

    /// The source lines (first, last) behind the preview line spanning
    /// chars `start..end`, newline excluded; None for a blank or unmapped
    /// line. Table rows go by their recorded row: padded columns defeat the
    /// char map.
    fn preview_source_lines(&self, start: i32, end: i32) -> Option<(i32, i32)> {
        if start == end {
            return None;
        }
        if let Some(row) = self.preview_table_rows.borrow().iter().find(|r| r.start <= start && start <= r.end) {
            let line = self.buffer.iter_at_offset(row.source?).line() + row.separator as i32;
            return Some((line, line));
        }
        let (a, b) = self.preview_map.borrow().source_span(start, end)?;
        Some((self.buffer.iter_at_offset(a).line(), self.buffer.iter_at_offset(b).line()))
    }

    /// Pick source lines from a click beside a preview line: the preview's
    /// own word or line selection gives way to the line-pick highlight.
    fn pick_from_preview(&self, (first, last): (i32, i32), extend: bool) {
        let preview = self.preview.buffer();
        if let Some((start, _)) = preview.selection_bounds() {
            preview.place_cursor(&start);
        }
        self.syncing_cursor.set(true);
        self.pick_source_lines(first, last, extend);
        self.syncing_cursor.set(false);
        self.center_editor_on_cursor();
    }

    /// Highlight the preview lines showing the gutter-picked source lines.
    /// Text maps through the source map, widened to whole preview lines;
    /// table rows, whose padded columns the source map can't place, go by
    /// their recorded row instead — any row whose source line is picked
    /// lights in full, and one that isn't never does.
    fn apply_line_pick(&self) {
        self.preview_gutter.queue_draw();
        let live = self.preview.buffer();
        let (all_start, all_end) = live.bounds();
        live.remove_tag_by_name("line-pick", &all_start, &all_end);
        let Some((start, end)) = self.line_pick.get() else {
            return;
        };
        if !self.preview_visible() {
            return;
        }
        let first = self.buffer.iter_at_offset(start).line();
        let end_iter = self.buffer.iter_at_offset(end);
        let last = if end_iter.starts_line() && end_iter.line() > first {
            end_iter.line() - 1
        } else {
            end_iter.line()
        };
        let picked = |line: i32| (first..=last).contains(&line);
        // Blank lines have nothing in the preview to point at.
        if self.buffer.text(&self.buffer.iter_at_offset(start), &end_iter, false).trim().is_empty() {
            return;
        }
        let total = live.char_count();
        let tag_lines = |a: i32, b: i32| {
            let mut a = live.iter_at_offset(a.clamp(0, total));
            let mut b = live.iter_at_offset(b.clamp(0, total));
            a.set_line_offset(0);
            if !b.ends_line() {
                b.forward_to_line_end();
            }
            live.apply_tag_by_name("line-pick", &a, &b);
        };
        let map = self.preview_map.borrow();
        // Map the last picked character, not the next line's start.
        let mut text_end = end_iter;
        if text_end.starts_line() && last < text_end.line() {
            text_end.backward_char();
        }
        if let (Some(a), Some(b)) = (map.preview_offset(start), map.preview_offset(text_end.offset())) {
            if b >= a {
                tag_lines(a, b);
            }
        }
        for row in self.preview_table_rows.borrow().iter() {
            let line = row.source.map(|s| self.buffer.iter_at_offset(s).line() + row.separator as i32);
            if line.is_some_and(&picked) {
                tag_lines(row.start, row.end);
            } else {
                live.remove_tag_by_name(
                    "line-pick",
                    &live.iter_at_offset(row.start),
                    &live.iter_at_offset(row.end),
                );
            }
        }
    }

    /// Size the gutter to the widest line number and redraw it.
    fn update_gutter(&self) {
        self.update_preview_gutter();
        if !self.line_numbers.get() {
            return;
        }
        let digits = self.buffer.line_count().to_string().len().max(2);
        let layout = self.editor.create_pango_layout(Some(&"8".repeat(digits)));
        let (w, _) = layout.pixel_size();
        self.gutter.set_content_width(w + 2 * GUTTER_PAD);
        self.gutter.queue_draw();
    }

    /// Attach or detach the source-line gutter on the preview's left window.
    fn apply_preview_line_numbers(&self, on: bool) {
        if on {
            self.preview.set_gutter(gtk4::TextWindowType::Left, Some(&self.preview_gutter));
            self.update_preview_gutter();
        } else {
            self.preview.set_gutter(gtk4::TextWindowType::Left, None::<&gtk4::Widget>);
        }
    }

    /// Size the preview gutter to the widest source line number and redraw.
    fn update_preview_gutter(&self) {
        if !self.preview_line_numbers.get() {
            return;
        }
        let digits = self.buffer.line_count().to_string().len().max(2);
        let layout = self.preview.create_pango_layout(Some(&"8".repeat(digits)));
        let (w, _) = layout.pixel_size();
        self.preview_gutter.set_content_width(w + 2 * GUTTER_PAD);
        self.preview_gutter.queue_draw();
    }

    /// Paint the preview gutter: each visible preview line gets the number
    /// of the first source line it was rendered from, in the preview's base
    /// font — dimmed unless the line is part of the current line pick. The
    /// number sits on the bottom of the line's first row, which lines it up
    /// with headings drawn larger; on a row much taller than the number (an
    /// image) it sits at the top instead.
    fn draw_preview_gutter(&self, area: &gtk4::DrawingArea, cr: &gtk4::cairo::Context, width: i32, height: i32) {
        let view = &self.preview;
        let live = view.buffer();
        let picked = live.tag_table().lookup("line-pick");
        let color = area.color();
        let (mut iter, _) = view.line_at_y(view.visible_rect().y());
        iter.set_line_offset(0);
        loop {
            let (y, line_height) = view.line_yrange(&iter);
            let (_, wy) = view.buffer_to_window_coords(gtk4::TextWindowType::Left, 0, y);
            if wy > height {
                break;
            }
            let mut end = iter;
            if !end.ends_line() {
                end.forward_to_line_end();
            }
            // Folded section bodies take no height and get no number.
            let lines = (line_height > 0)
                .then(|| self.preview_source_lines(iter.offset(), end.offset()))
                .flatten();
            if let Some((first, _)) = lines {
                let lit = picked.as_ref().is_some_and(|tag| iter.has_tag(tag));
                cr.set_source_rgba(
                    color.red() as f64,
                    color.green() as f64,
                    color.blue() as f64,
                    color.alpha() as f64 * if lit { 1.0 } else { 0.45 },
                );
                let layout = view.create_pango_layout(Some(&(first + 1).to_string()));
                let (lw, lh) = layout.pixel_size();
                let row = view.iter_location(&iter);
                let (_, row_y) = view.buffer_to_window_coords(gtk4::TextWindowType::Left, 0, row.y());
                let top = if row.height() > 3 * lh { row_y } else { row_y + row.height() - lh };
                cr.move_to((width - GUTTER_PAD - lw) as f64, top as f64);
                pangocairo::functions::show_layout(cr, &layout);
            }
            // As in the editor gutter: an empty last line still counts.
            let line = iter.line();
            if !iter.forward_line() && iter.line() == line {
                break;
            }
        }
    }

    /// Repaint everything the palette touches: the widget chrome (via the
    /// stylesheet) and the text tags inside the two buffers, which GTK styles
    /// from `TextTag` properties and so the stylesheet cannot reach.
    ///
    /// Resolved against `StyleManager::is_dark` rather than the `color_scheme`
    /// setting, because under `ColorScheme::System` only the style manager
    /// knows which way the desktop has gone.
    fn apply_theme(self: &Rc<Self>) {
        let dark = adw::StyleManager::default().is_dark();
        let theme = self.theme.get();
        self.theme_provider.load_from_string(&theme::stylesheet(theme, dark));
        let colors = theme::tags(theme, dark);
        highlight::retint(&self.buffer, colors);
        preview::retint(&self.preview.buffer(), colors);
    }

    /// How far "Fit preview to width" shrinks the preview: the panel's width
    /// against `FIT_EMS` of the chosen preview font, so a narrow panel shows
    /// the same words per line as a full-width one, only smaller. Never
    /// enlarges — a wide preview (view mode) keeps the chosen size.
    fn fit_scale(&self) -> f64 {
        if !self.preview_fit.get() {
            return 1.0;
        }
        let width = self.preview.width() - self.preview.left_margin() - self.preview.right_margin();
        if width <= 0 {
            return self.preview_scale.get();
        }
        let em_px = self.preview_font.get().preview_pt() / 0.75;
        (width as f64 / (em_px * FIT_EMS)).min(1.0)
    }

    /// Re-fit the preview font after a width change. Debounced, since every
    /// font change re-lays out the whole preview and dragging a divider
    /// changes the width every frame — except the first fit after launch,
    /// which runs at once so the preview doesn't open at full size.
    fn schedule_fit(self: &Rc<Self>) {
        if !self.preview_fit.get() {
            return;
        }
        if let Some(id) = self.fit_pending.take() {
            id.remove();
        }
        let delay = if self.preview_scale.get() == 1.0 { 0 } else { 120 };
        let state = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            state.fit_pending.take();
            let font = state.preview_font.get();
            if font.fitted_preview_pt(state.fit_scale())
                != font.fitted_preview_pt(state.preview_scale.get())
            {
                state.apply_fonts();
            }
        });
        self.fit_pending.set(Some(id));
    }

    fn apply_fonts(self: &Rc<Self>) {
        let scale = self.fit_scale();
        self.preview_scale.set(scale);
        let font = self.preview_font.get();
        let pt = font.fitted_preview_pt(scale);
        // Images follow the size actually applied, floor and rounding included.
        let images_changed = preview::set_image_scale(pt / font.preview_pt());
        let mut css = format!("textview.preview-view {{ font-size: {pt}pt; }}");
        // BINDLE_EDITOR_FONT (uishot experiments) keeps the last word.
        if std::env::var("BINDLE_EDITOR_FONT").is_err() {
            css.push_str(&format!(
                "textview.editor-view {{ font-size: {}; }}",
                self.editor_font.get().pt()
            ));
        }
        self.font_provider.load_from_string(&css);
        // The new style lands on a later frame; size the gutter after it so
        // its numbers measure with the font they will be drawn in.
        let state = self.clone();
        glib::idle_add_local_once(move || {
            state.update_gutter();
            if images_changed && state.preview_visible() {
                state.schedule_preview();
            } else {
                state.refit_tables();
            }
        });
    }

    /// Forget the highlight baseline, so the next `apply_highlight` re-tags
    /// the whole buffer. Needed whenever the text under the tags changed
    /// without this pass seeing it — a new document, or a new document kind.
    fn reset_highlight(&self) {
        self.hl_sigs.borrow_mut().clear();
    }

    /// Re-apply keyword highlighting to the editor buffer, or strip it when
    /// disabled. Tag changes touch neither the undo stack nor the modified
    /// state.
    ///
    /// Only the lines whose highlighting differs from the last pass are
    /// touched. Scanning is cheap; *tagging* is not — every `remove_tag` and
    /// `apply_tag` invalidates the layout of the lines it covers, and GTK
    /// re-validates them before the next frame, so re-tagging a whole
    /// document on each debounced keystroke is what made large files crawl.
    /// A one-line edit now costs one line of tag work however long the
    /// document is; see `highlight::line_sigs`.
    fn apply_highlight(&self) {
        let clear_all = |buffer: &gtk4::TextBuffer| {
            let (start, end) = (buffer.start_iter(), buffer.end_iter());
            for tag in highlight::TAGS {
                buffer.remove_tag_by_name(tag, &start, &end);
            }
        };
        if !self.keyword_highlight.get() {
            clear_all(&self.buffer);
            self.reset_highlight();
            return;
        }
        let text = self.text();
        // Past this size the O(document) *scan* starts to show on every
        // debounce, incremental tagging or not. Nothing here can be done
        // per-line — the scanners take the whole text — so a file this big
        // simply goes unhighlighted rather than making it unusable to type in.
        if text.len() > HIGHLIGHT_MAX_BYTES {
            if !self.hl_sigs.borrow().is_empty() {
                clear_all(&self.buffer);
                self.reset_highlight();
            }
            return;
        }
        let spans = highlight::spans(&text, self.doc_kind());
        let (starts, sigs) = highlight::line_sigs(&text, &spans);
        let dirty = {
            let baseline = self.hl_sigs.borrow();
            // Empty, or somehow out of step with the buffer: tag everything.
            if baseline.len() != sigs.len() {
                Some((0, sigs.len() - 1))
            } else {
                highlight::dirty_lines(&baseline, &sigs)
            }
        };
        *self.hl_sigs.borrow_mut() = sigs;
        let Some((lo, hi)) = dirty else {
            return;
        };
        // Char offsets of the dirty line range: from its first line's start
        // to the end of its last line (or the buffer end).
        let Some(&from) = starts.get(lo) else {
            return;
        };
        let to = starts.get(hi + 1).copied().unwrap_or_else(|| self.buffer.end_iter().offset());
        let (a, b) = (self.buffer.iter_at_offset(from), self.buffer.iter_at_offset(to));
        for tag in highlight::TAGS {
            self.buffer.remove_tag_by_name(tag, &a, &b);
        }
        // Spans are ordered by line, so the dirty ones are a contiguous run.
        for span in spans.iter().skip_while(|s| s.start < from).take_while(|s| s.start < to) {
            self.buffer.apply_tag_by_name(
                span.tag,
                &self.buffer.iter_at_offset(span.start),
                &self.buffer.iter_at_offset(span.end),
            );
        }
    }

    // --- HTML / PDF export ---

    /// Suggested export file name: the document's stem with `ext`.
    fn export_doc_name(&self, ext: &str) -> String {
        let stem = self
            .source
            .borrow()
            .container()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "Untitled".to_string());
        format!("{stem}.{ext}")
    }

    /// Bytes of a local image reference, for embedding in exports.
    fn resolve_asset_bytes(&self, dest: &str) -> Option<Vec<u8>> {
        self.asset_loc(dest)?.read(bundle::ASSET_MAX_BYTES)
    }

    /// Where a local reference's bytes live, worked out without touching
    /// the disk, so the read itself can run on a worker thread.
    fn asset_loc(&self, dest: &str) -> Option<AssetLoc> {
        if external_link(dest) || dest.starts_with('#') || dest.starts_with("data:") {
            return None;
        }
        let clean = bundle::decode_dest(dest);
        if let Some(staged) = self.staged_assets.borrow().get(&clean) {
            return Some(match staged {
                bundle::AssetSource::Bytes(bytes) => AssetLoc::Bytes(bytes.clone()),
                bundle::AssetSource::File(path) => AssetLoc::File(path.clone()),
            });
        }
        let source = self.source.borrow().clone();
        if let Some((path, info)) = source.archive() {
            return Some(AssetLoc::Archive(path, info, clean));
        }
        let raw = Path::new(&clean);
        Some(AssetLoc::File(if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            source.preview_base()?.join(raw)
        }))
    }

    fn export_html_dialog(self: &Rc<Self>) {
        let dialog = gtk4::FileDialog::builder()
            .title("Export as HTML")
            .initial_name(self.export_doc_name("html"))
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                let resolve = |dest: &str| state.resolve_asset_bytes(dest);
                let content = if state.doc_kind().is_markdown() {
                    export::Content::Markdown { resolve: &resolve }
                } else {
                    export::Content::Plain
                };
                let html = export::html_document(&file_stem_label(&path), &state.text(), content);
                match doc::write_atomic(&path, html.as_bytes()) {
                    Ok(()) => state.toast(&format!("Exported {}", file_label(&path))),
                    Err(err) => state.toast(&format!("Could not export: {err}")),
                }
            }
        });
    }

    /// What the document is called in the printer queue.
    fn print_job_name(&self) -> String {
        self.source
            .borrow()
            .container()
            .map(|p| file_stem_label(&p))
            .unwrap_or_else(|| "Untitled".to_string())
    }

    /// The document as PDF bytes laid out for `page` — what both printing
    /// and (via `write_pdf`) exporting put on paper.
    fn document_pdf(&self, page: export::Page) -> Result<Vec<u8>, String> {
        let resolve = |dest: &str| self.resolve_asset_bytes(dest);
        let content = if self.doc_kind().is_markdown() {
            export::Content::Markdown { resolve: &resolve }
        } else {
            export::Content::Plain
        };
        export::pdf_bytes(&self.print_job_name(), &self.text(), content, page)
    }

    /// Print: ask for printer and paper first (`setup`), render the PDF for
    /// exactly that sheet, then spool it to the stream `print` hands back.
    /// Nothing touches the filesystem — the dialog wants PDF bytes, so the
    /// document never becomes a temp file that could be left behind.
    fn print_dialog(self: &Rc<Self>) {
        let dialog = gtk4::PrintDialog::new();
        dialog.set_title(&self.print_job_name());
        dialog.set_modal(true);
        let state = self.clone();
        let picker = dialog.clone();
        dialog.setup(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let setup = match result {
                Ok(setup) => setup,
                Err(err) => return state.print_failed(&err),
            };
            let bytes = match state.document_pdf(page_geometry(&setup.page_setup())) {
                Ok(bytes) => bytes,
                Err(err) => return state.toast(&format!("Could not prepare the document: {err}")),
            };
            let window = state.window.clone();
            let state = state.clone();
            picker.print(Some(&window), Some(&setup), gio::Cancellable::NONE, move |result| {
                let stream = match result {
                    Ok(stream) => stream,
                    Err(err) => return state.print_failed(&err),
                };
                // Async: the stream is a pipe to the print backend, and a
                // full pipe buffer would otherwise stall the window.
                let sink = stream.clone();
                stream.write_all_async(
                    bytes,
                    glib::Priority::DEFAULT,
                    gio::Cancellable::NONE,
                    move |result| {
                        // A short write reports its error in the third slot.
                        let spooled = match result {
                            Ok((_, _, Some(err))) | Err((_, err)) => Err(err),
                            Ok(_) => sink.close(gio::Cancellable::NONE),
                        };
                        match spooled {
                            // Not "sent to the printer": the print portal
                            // decides what the document becomes — a spooled
                            // job, a file, or a preview if that button was
                            // used — and tells the app none of it.
                            Ok(()) => state.toast("Sent to the print system"),
                            Err(err) => {
                                state.toast(&format!("Could not print: {}", err.message()))
                            }
                        }
                    },
                );
            });
        });
    }

    /// Dismissing the print dialog is not a failure; anything else is.
    fn print_failed(&self, err: &glib::Error) {
        if !err.matches(gtk4::DialogError::Dismissed) && !err.matches(gtk4::DialogError::Cancelled) {
            self.toast(&format!("Could not print: {}", err.message()));
        }
    }

    fn export_pdf_dialog(self: &Rc<Self>) {
        let dialog = gtk4::FileDialog::builder()
            .title("Export as PDF")
            .initial_name(self.export_doc_name("pdf"))
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                let resolve = |dest: &str| state.resolve_asset_bytes(dest);
                let content = if state.doc_kind().is_markdown() {
                    export::Content::Markdown { resolve: &resolve }
                } else {
                    export::Content::Plain
                };
                let written =
                    export::write_pdf(&path, &file_stem_label(&path), &state.text(), content);
                match written {
                    Ok(()) => state.toast(&format!("Exported {}", file_label(&path))),
                    Err(err) => state.toast(&format!("Could not export: {err}")),
                }
            }
        });
    }

    fn export_odt_dialog(self: &Rc<Self>) {
        let dialog = gtk4::FileDialog::builder()
            .title("Export as ODT")
            .initial_name(self.export_doc_name("odt"))
            .build();
        let state = self.clone();
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Some(path) = state.picked_path(result) {
                // Exporting over the imported original is exactly the lossy
                // write-back that importing refuses to do.
                if matches!(&*state.source.borrow(), DocSource::Odt { path: origin } if *origin == path) {
                    return state.toast("That is the imported original — choose another name");
                }
                let resolve = |dest: &str| state.resolve_asset_bytes(dest);
                let content = if state.doc_kind().is_markdown() {
                    export::Content::Markdown { resolve: &resolve }
                } else {
                    export::Content::Plain
                };
                let written = odf::write_odt(&path, &file_stem_label(&path), &state.text(), content);
                match written {
                    Ok(()) => state.toast(&format!("Exported {}", file_label(&path))),
                    Err(err) => state.toast(&format!("Could not export: {err}")),
                }
            }
        });
    }

    /// Run `then` immediately if the document is clean, otherwise ask first.
    fn guard_unsaved(self: &Rc<Self>, then: Box<dyn FnOnce()>) {
        if !self.modified.get() {
            then();
            return;
        }
        // One question at a time: a second close request (Alt+F4 again) or
        // open while the dialog is up would stack another one over it.
        if self.guard_active.get() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Save Changes?"),
            Some("The document has unsaved changes."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("discard", "_Discard"), ("save", "_Save")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        self.guard_active.set(true);
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            state.guard_active.set(false);
            match response.as_str() {
                "save" => state.save(Some(then)),
                "discard" => then(),
                _ => {}
            }
        });
    }

    /// Run `then` unless the document is a checked-out memo with changes
    /// that were never pushed; ask first in that case. Mirrors
    /// `guard_unsaved` and composes with it — nothing is lost either way,
    /// since the local cache file keeps the text.
    fn guard_unpushed(self: &Rc<Self>, then: Box<dyn FnOnce()>) {
        if !self.memo_unpushed.get() || self.memo_doc().is_none() {
            then();
            return;
        }
        if self.guard_active.get() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Push to Memos?"),
            Some(
                "This memo has changes that exist only on this computer. \
                 They stay in the local copy either way.",
            ),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("later", "_Not Now"), ("push", "_Push")]);
        dialog.set_response_appearance("push", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("push"));
        dialog.set_close_response("cancel");
        self.guard_active.set(true);
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            state.guard_active.set(false);
            match response.as_str() {
                "push" => state.memos_push(Some(then)),
                "later" => then(),
                _ => {}
            }
        });
    }

    // --- Memos ---

    /// Run a Memos request on a worker thread, then hand the result to
    /// `done` back on the main loop. Every network call goes through here:
    /// an unreachable server blocks for the full timeout, which on the main
    /// thread would freeze the window — the same reason `open_path` reads
    /// files off the main loop.
    fn memos_call<T, F, D>(self: &Rc<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce(&memos::Server) -> Result<T, memos::Error> + Send + 'static,
        D: FnOnce(&Rc<App>, Result<T, memos::Error>) + 'static,
    {
        let base = self.memos_url.borrow().clone();
        let insecure = self.memos_insecure.get();
        let state = self.clone();
        glib::spawn_future_local(async move {
            let joined = gio::spawn_blocking(move || {
                memos_server(base, insecure).and_then(|s| work(&s))
            })
            .await;
            let result = joined
                .unwrap_or_else(|_| Err(memos::Error::Network("the request failed".into())));
            done(&state, result);
        });
    }

    /// True once a server is set; the token's absence is reported by the
    /// request itself, since checking it would mean a keyring round trip.
    fn memos_ready(&self) -> bool {
        if self.memos_url.borrow().is_empty() {
            self.toast(&memos::Error::NoServer.to_string());
            return false;
        }
        true
    }

    /// The memo link of the current document, if it is a checked-out memo.
    fn memo_doc(&self) -> Option<MemoDoc> {
        let path = self.source.borrow().container().map(Path::to_path_buf)?;
        self.memo_doc_for(&path)
    }

    fn memo_doc_for(&self, path: &Path) -> Option<MemoDoc> {
        self.memo_docs.borrow().iter().find(|d| d.path == path).cloned()
    }

    fn memo_doc_for_name(&self, name: &str) -> Option<MemoDoc> {
        self.memo_docs.borrow().iter().find(|d| d.name == name).cloned()
    }

    fn remember_memo_doc(&self, doc: MemoDoc) {
        settings::remember_memo(&mut self.memo_docs.borrow_mut(), doc);
    }

    /// Delete checked-out memos that have gone stale in the cache
    /// directory. Runs when the browse dialog opens — the moment you are
    /// about to check more out — rather than at startup, so it costs launch
    /// time nothing; the directory read and the unlinks happen on a worker
    /// thread all the same.
    ///
    /// The guard list is built here, on the main loop, where the open
    /// document and the store can actually be read: everything currently
    /// open or holding unpushed edits is protected, and `memo_cache_stale`
    /// applies the rest of the rules. Anything deleted can be fetched from
    /// the server again.
    fn memos_sweep_cache(&self) {
        let mut protected: Vec<PathBuf> =
            self.memo_docs.borrow().iter().filter(|d| d.unpushed).map(|d| d.path.clone()).collect();
        protected.extend(self.source.borrow().container().map(Path::to_path_buf));
        gio::spawn_blocking(move || {
            let Ok(entries) = fs::read_dir(memo_cache_dir()) else {
                return;
            };
            let now = std::time::SystemTime::now();
            for entry in entries.flatten() {
                let path = entry.path();
                let age = entry
                    .metadata()
                    .ok()
                    .filter(|meta| meta.is_file())
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|modified| now.duration_since(modified).ok());
                // No age means no readable mtime, or a clock skewed into the
                // future: leave the file alone rather than guess.
                if age.is_some_and(|age| settings::memo_cache_stale(&path, age, &protected)) {
                    let _ = fs::remove_file(&path);
                }
            }
        });
    }

    /// Keep the stored entry for the current document in step with the live
    /// unpushed flag, the way `sync_doc_state` does for folds.
    fn sync_memo_state(&self) {
        let Some(path) = self.source.borrow().container().map(Path::to_path_buf) else {
            return;
        };
        let unpushed = self.memo_unpushed.get();
        let mut docs = self.memo_docs.borrow_mut();
        if let Some(doc) = docs.iter_mut().find(|d| d.path == path) {
            doc.unpushed = unpushed;
        }
    }

    /// Grey out the actions that do nothing for the current kind: both panel
    /// toggles and view mode outside markdown, and word wrap for NFO/DIZ art.
    /// `apply_panels`/`apply_wrap` already override the effect — this is so
    /// the menu says why, instead of offering a toggle that visibly does
    /// nothing.
    fn update_kind_actions(&self) {
        let kind = self.doc_kind();
        let enabled = kind.uses_panels();
        for (name, enabled) in [
            ("show-insert", enabled),
            ("show-preview", enabled),
            ("view-mode", enabled),
            ("preview-line-numbers", enabled),
            ("preview-fit", enabled),
            ("word-wrap", kind.wrap_lines()),
        ] {
            if let Some(action) = self.window.lookup_action(name) {
                if let Ok(action) = action.downcast::<gio::SimpleAction>() {
                    action.set_enabled(enabled);
                }
            }
        }
    }

    /// Editor wrap follows the "Word wrap" toggle, except that NFO/DIZ art
    /// never wraps — the override is applied here and never persisted.
    fn apply_wrap(&self) {
        let wrap = self.word_wrap.get() && self.doc_kind().wrap_lines();
        self.editor.set_wrap_mode(if wrap {
            gtk4::WrapMode::WordChar
        } else {
            gtk4::WrapMode::None
        });
        // Line heights just changed; a document that fits without scrolling
        // emits no adjustment signal, so the gutter would keep old positions.
        self.gutter.queue_draw();
    }

    /// Grey out "Push to Memos" for documents that did not come from Memos.
    fn update_memos_actions(&self) {
        let enabled = self.memo_doc().is_some();
        if let Some(action) = self.window.lookup_action("memos-push") {
            if let Ok(action) = action.downcast::<gio::SimpleAction>() {
                action.set_enabled(enabled);
            }
        }
    }

    /// Server URL and access token. The token field starts blank and is
    /// never prefilled from the keyring: reading it back would put the
    /// secret on screen for no benefit, and leaving it empty is how the
    /// user says "keep the one already saved".
    fn memos_setup_dialog(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Memos Server"),
            Some(
                "Bindle signs in with an access token, not a password. \
                 Create one in the Memos web interface under \
                 Settings → Access Tokens.",
            ),
        );
        let group = adw::PreferencesGroup::new();
        group.set_description(Some("Leave the token blank to keep the one already saved."));
        let url_row = adw::EntryRow::builder().title("Server address").build();
        url_row.set_text(&self.memos_url.borrow());
        let token_row = adw::PasswordEntryRow::builder().title("Access token").build();
        // Last resort for a LAN server whose CA can't be installed on this
        // machine. Spelled out rather than labelled "insecure", so the
        // trade is visible at the switch instead of in a wiki somewhere.
        let insecure_row = adw::SwitchRow::builder()
            .title("Skip certificate checks")
            .subtitle(
                "Accepts any certificate. Anything able to route your traffic \
                 could then read the access token. Prefer installing your CA.",
            )
            .active(self.memos_insecure.get())
            .build();
        group.add(&url_row);
        group.add(&token_row);
        group.add(&insecure_row);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[
            ("cancel", "_Cancel"),
            ("forget", "_Forget Token"),
            ("save", "_Save"),
        ]);
        dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            match response.as_str() {
                "save" => {
                    let base = memos::normalize_base(&url_row.text());
                    if base.is_empty() {
                        return state.toast("Enter the address of your Memos server");
                    }
                    *state.memos_url.borrow_mut() = base;
                    let insecure = insecure_row.is_active();
                    let newly_insecure = insecure && !state.memos_insecure.replace(insecure);
                    state.persist_settings();
                    state.update_title();
                    if newly_insecure {
                        state.toast("Certificate checks are off for Memos");
                    }
                    state.memos_save_and_test(token_row.text().to_string());
                }
                "forget" => state.memos_forget_token(),
                _ => {}
            }
        });
    }

    /// Store the token (when one was typed) and prove the pair works, so a
    /// typo surfaces here rather than at the next push.
    fn memos_save_and_test(self: &Rc<Self>, token: String) {
        self.toast("Contacting Memos…");
        let base = self.memos_url.borrow().clone();
        let insecure = self.memos_insecure.get();
        let state = self.clone();
        glib::spawn_future_local(async move {
            let joined = gio::spawn_blocking(move || {
                if !token.is_empty() {
                    secrets::store_token(&token).map_err(memos::Error::Keyring)?;
                }
                memos::check(&memos_server(base, insecure)?)
            })
            .await;
            match joined {
                Ok(Ok(_)) => state.toast("Connected to Memos"),
                Ok(Err(err)) => state.toast(&err.to_string()),
                Err(_) => state.toast("Could not reach Memos"),
            }
        });
    }

    fn memos_forget_token(self: &Rc<Self>) {
        let state = self.clone();
        glib::spawn_future_local(async move {
            match gio::spawn_blocking(secrets::clear_token).await {
                Ok(Ok(())) => state.toast("Access token removed from the keyring"),
                Ok(Err(err)) => state.toast(&format!("Could not remove the token: {err}")),
                Err(_) => state.toast("Could not remove the token"),
            }
        });
    }

    /// Post the selection — or the whole document — as a new memo.
    fn memos_send_dialog(self: &Rc<Self>) {
        if !self.memos_ready() {
            return;
        }
        let (content, scope) = match self.buffer.selection_bounds() {
            Some((start, end)) => {
                (self.buffer.text(&start, &end, false).to_string(), "the selection")
            }
            None => (self.text().to_string(), "the whole document"),
        };
        if content.trim().is_empty() {
            return self.toast("Nothing to send — there is no text");
        }
        let dialog = adw::AlertDialog::new(
            Some("Send to Memos"),
            Some(&format!(
                "Create a new memo from {scope} ({} characters).",
                content.chars().count()
            )),
        );
        let group = adw::PreferencesGroup::new();
        let visibility = adw::ComboRow::new();
        visibility.set_title("Visibility");
        let labels: Vec<&str> = memos::Visibility::ALL.iter().map(|v| v.label()).collect();
        visibility.set_model(Some(&gtk4::StringList::new(&labels)));
        visibility.set_selected(0);
        group.add(&visibility);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("send"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response.as_str() != "send" {
                return;
            }
            let chosen = memos::Visibility::ALL
                .get(visibility.selected() as usize)
                .copied()
                .unwrap_or(memos::Visibility::Private);
            state.toast("Sending to Memos…");
            state.memos_call(
                move |server| memos::create(server, &content, chosen),
                |state, result| match result {
                    Ok(memo) => state.toast(&format!(
                        "Sent to Memos ({})",
                        memo.visibility.short_label().to_lowercase()
                    )),
                    Err(err) => state.toast(&err.to_string()),
                },
            );
        });
    }

    /// Browse the server's memos and open one for editing.
    fn memos_browse_dialog(self: &Rc<Self>) {
        if !self.memos_ready() {
            return;
        }
        self.memos_sweep_cache();
        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Search memos…"));
        search.set_hexpand(true);
        let search_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        search_box.set_margin_start(12);
        search_box.set_margin_end(12);
        search_box.set_margin_top(6);
        search_box.set_margin_bottom(6);
        search_box.append(&search);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.add_css_class("boxed-list");

        let status = gtk4::Label::new(None);
        status.add_css_class("dim-label");
        status.set_wrap(true);
        status.set_margin_top(24);
        status.set_margin_start(12);
        status.set_margin_end(12);

        let spinner = gtk4::Spinner::new();
        spinner.set_margin_top(24);

        let more = gtk4::Button::with_label("Load more");
        more.add_css_class("flat");
        more.set_margin_top(6);
        more.set_visible(false);

        let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        column.set_margin_start(12);
        column.set_margin_end(12);
        column.set_margin_bottom(12);
        column.append(&spinner);
        column.append(&status);
        column.append(&list);
        column.append(&more);
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&column)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();

        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content.append(&search_box);
        content.append(&scroll);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        let dialog = adw::Dialog::new();
        dialog.set_title("Memos");
        dialog.set_content_width(580);
        dialog.set_content_height(640);
        dialog.set_child(Some(&toolbar));

        let browse = Rc::new(Browse {
            dialog: dialog.clone(),
            list: list.clone(),
            status,
            more: more.clone(),
            spinner,
            memos: RefCell::new(Vec::new()),
            next_token: RefCell::new(String::new()),
            query: RefCell::new(String::new()),
            loading: Cell::new(false),
        });

        // Searching restarts paging from the first page.
        let state = self.clone();
        let b = browse.clone();
        search.connect_search_changed(move |entry| {
            *b.query.borrow_mut() = entry.text().to_string();
            state.memos_load_page(&b, false);
        });

        let state = self.clone();
        let b = browse.clone();
        more.connect_clicked(move |_| state.memos_load_page(&b, true));

        let state = self.clone();
        let b = browse.clone();
        list.connect_row_activated(move |_, row| {
            let Some(memo) = b.memos.borrow().get(row.index() as usize).cloned() else {
                return;
            };
            b.dialog.close();
            let s = state.clone();
            state.guard_unsaved(Box::new(move || s.memos_open(memo)));
        });

        dialog.present(Some(&self.window));
        self.memos_load_page(&browse, false);
    }

    /// Fetch one page into the browse dialog. `append` adds to the list;
    /// otherwise it replaces it (a new search, or the first load).
    fn memos_load_page(self: &Rc<Self>, browse: &Rc<Browse>, append: bool) {
        if browse.loading.replace(true) {
            return;
        }
        browse.spinner.set_visible(true);
        browse.spinner.start();
        browse.more.set_sensitive(false);
        if !append {
            browse.status.set_visible(false);
        }
        let token = if append { browse.next_token.borrow().clone() } else { String::new() };
        let query = browse.query.borrow().clone();
        let asked = query.clone();
        let browse = browse.clone();
        self.memos_call(
            move |server| memos::list(server, &token, &query),
            move |state, result| {
                browse.loading.set(false);
                // The search changed while this page was loading, and that
                // request was turned away above: this page answers an old
                // query, so fetch the current one instead of showing it.
                if *browse.query.borrow() != asked {
                    return state.memos_load_page(&browse, false);
                }
                browse.spinner.stop();
                browse.spinner.set_visible(false);
                browse.more.set_sensitive(true);
                match result {
                    Ok((page, next)) => state.memos_show_page(&browse, page, next, append),
                    Err(err) => {
                        browse.status.set_label(&err.to_string());
                        browse.status.set_visible(true);
                    }
                }
            },
        );
    }

    fn memos_show_page(
        self: &Rc<Self>,
        browse: &Rc<Browse>,
        page: Vec<memos::Memo>,
        next: String,
        append: bool,
    ) {
        if !append {
            while let Some(row) = browse.list.first_child() {
                browse.list.remove(&row);
            }
            browse.memos.borrow_mut().clear();
        }
        for memo in &page {
            let row = adw::ActionRow::new();
            let title = memos::summary(&memo.content);
            // ActionRow titles are markup: escape, or a memo containing an
            // ampersand or angle bracket renders wrong (or not at all).
            row.set_title(&glib::markup_escape_text(if title.is_empty() {
                "(empty memo)"
            } else {
                &title
            }));
            row.set_subtitle(&format!(
                "{} · {}",
                memos::short_date(&memo.update_time),
                memo.visibility.short_label()
            ));
            // One line each, so rows keep a uniform height however long the
            // memo's first line is; the full text is one click away anyway.
            row.set_title_lines(1);
            row.set_subtitle_lines(1);
            row.set_activatable(true);
            browse.list.append(&row);
        }
        browse.memos.borrow_mut().extend(page);
        *browse.next_token.borrow_mut() = next.clone();
        browse.more.set_visible(!next.is_empty());
        let empty = browse.memos.borrow().is_empty();
        browse.list.set_visible(!empty);
        browse.status.set_visible(empty);
        if empty {
            browse.status.set_label(if browse.query.borrow().trim().is_empty() {
                "No memos on the server yet"
            } else {
                "No memos match that search"
            });
        }
    }

    /// Check a memo out into a local cache file and open it as an ordinary
    /// document. Bindle is path-based end to end — `DocSource`, folds, find,
    /// save — so a fetched memo becomes a real file rather than a new kind
    /// of document, and everything downstream keeps working unchanged.
    fn memos_open(self: &Rc<Self>, memo: memos::Memo) {
        // Never clobber a local draft with the server's copy: if this memo
        // has unpushed edits, open those instead and say so.
        if let Some(doc) = self.memo_doc_for_name(&memo.name) {
            if doc.unpushed && doc.path.is_file() {
                self.toast("Opened your unpushed local copy of this memo");
                return self.open_path(&doc.path);
            }
        }
        let path = match self.memo_cache_path(&memo) {
            Ok(path) => path,
            Err(err) => return self.toast(&format!("Could not prepare the local copy: {err}")),
        };
        if let Err(err) = doc::write_atomic(&path, memo.content.as_bytes()) {
            return self.toast(&format!("Could not write {}: {err}", path.display()));
        }
        self.remember_memo_doc(MemoDoc {
            path: path.clone(),
            name: memo.name,
            update_time: memo.update_time,
            unpushed: false,
        });
        self.open_path(&path);
    }

    /// Where a memo is checked out: `~/.cache/bindle/memos/<slug>-<uid>.md`.
    /// A remembered path is reused so the file stays put when the memo's
    /// first line — and with it the slug — changes.
    fn memo_cache_path(&self, memo: &memos::Memo) -> std::io::Result<PathBuf> {
        let path = match self.memo_doc_for_name(&memo.name) {
            Some(doc) => doc.path,
            None => memo_cache_dir().join(format!("{}.md", memos::cache_stem(memo))),
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        Ok(path)
    }

    /// Push the buffer back to the memo it was checked out from, saving the
    /// local copy first so the text survives a refused or failed push.
    fn memos_push(self: &Rc<Self>, then: Option<Box<dyn FnOnce()>>) {
        let Some(doc) = self.memo_doc() else {
            return self
                .toast("This document is not from Memos — use Send to Memos to create one");
        };
        if self.modified.get() {
            if let Err(err) = doc::write_atomic(&doc.path, self.text().as_bytes()) {
                return self.toast(&format!("Could not save the local copy: {err}"));
            }
            self.restamp();
            self.modified.set(false);
            self.update_title();
        }
        self.toast("Pushing to Memos…");
        self.memos_push_request(doc.name, self.text().to_string(), Some(doc.update_time), then);
    }

    /// One push attempt. `expected` carries the timestamp the memo was
    /// fetched at, so `memos::push` can refuse to overwrite an edit made
    /// elsewhere; `None` is the second pass, after the user chose to.
    fn memos_push_request(
        self: &Rc<Self>,
        name: String,
        content: String,
        expected: Option<String>,
        then: Option<Box<dyn FnOnce()>>,
    ) {
        let body = content.clone();
        let target = name.clone();
        self.memos_call(
            move |server| memos::push(server, &target, &body, expected.as_deref()),
            move |state, result| match result {
                Ok(memos::Push::Done(memo)) => {
                    if state.memo_pushed(&memo, &content) {
                        state.toast("Pushed to Memos");
                        if let Some(then) = then {
                            then();
                        }
                    } else {
                        // Not running `then`: when that is closing the
                        // window, the edits made meanwhile would go with it.
                        state.toast("Pushed — but the memo was edited meanwhile; push again");
                    }
                }
                Ok(memos::Push::Conflict(current)) => {
                    state.memos_conflict_dialog(name, content, current, then)
                }
                Err(err) => state.toast(&err.to_string()),
            },
        );
    }

    /// Record a successful push of `content`: the server's new timestamp
    /// becomes the one the next push compares against. Returns whether the
    /// local copy is now fully pushed.
    ///
    /// The push ran asynchronously, so the local side may have moved on:
    /// typing continued, or another document was opened. Only when the memo's
    /// text still equals what was sent is it marked pushed — clearing the flag
    /// otherwise lets the next Browse overwrite those edits with the server's
    /// copy, and would clear it on whichever memo happens to be open now.
    fn memo_pushed(&self, memo: &memos::Memo, content: &str) -> bool {
        let Some(doc) = self.memo_doc_for_name(&memo.name) else {
            return true;
        };
        let current = self.memo_doc().is_some_and(|d| d.name == memo.name);
        let settled = if current {
            self.text().as_str() == content
        } else {
            fs::read_to_string(&doc.path).is_ok_and(|text| text == content)
        };
        {
            let mut docs = self.memo_docs.borrow_mut();
            if let Some(doc) = docs.iter_mut().find(|d| d.name == memo.name) {
                doc.update_time = memo.update_time.clone();
                if settled {
                    doc.unpushed = false;
                }
            }
        }
        if current && settled {
            self.memo_unpushed.set(false);
        }
        self.update_title();
        self.persist_settings();
        settled
    }

    fn memos_conflict_dialog(
        self: &Rc<Self>,
        name: String,
        content: String,
        current: memos::Memo,
        then: Option<Box<dyn FnOnce()>>,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Memo Changed on the Server"),
            Some(&format!(
                "This memo was edited elsewhere on {}. Pushing now replaces \
                 that version with the one on this computer.",
                memos::short_date(&current.update_time)
            )),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("overwrite", "_Overwrite")]);
        dialog.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let state = self.clone();
        dialog.choose(Some(&self.window), gio::Cancellable::NONE, move |response| {
            if response.as_str() == "overwrite" {
                state.memos_push_request(name, content, None, then);
            }
        });
    }

    // --- Misc ---

    fn update_title(&self) {
        let source = self.source.borrow();
        let file = source.container();
        let name = file
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".to_string());
        let marker = if self.modified.get() { "• " } else { "" };
        self.title.set_title(&format!("{marker}{name}"));
        let subtitle = match file.and_then(|p| self.memo_doc_for(p)) {
            // A checked-out memo's real home is the server; the cache path
            // it happens to live at locally would say nothing useful.
            Some(_) => {
                let unpushed = if self.memo_unpushed.get() { " — unpushed changes" } else { "" };
                // Keep an unverified connection visible while it is in use,
                // not just at the switch that turned it on.
                let unverified = if self.memos_insecure.get() { " (unverified)" } else { "" };
                format!(
                    "Memos · {}{unverified}{unpushed}",
                    server_host(&self.memos_url.borrow())
                )
            }
            None => file.and_then(|p| p.parent()).map(display_dir).unwrap_or_default(),
        };
        // Saving an imported .odt writes a new file; say so where it shows.
        let subtitle = if matches!(*source, DocSource::Odt { .. }) {
            format!("{subtitle} · imported")
        } else {
            subtitle
        };
        self.title.set_subtitle(&subtitle);
        drop(source);
        self.mark_current_file();
    }

    fn persist_settings(&self) {
        self.sync_doc_state();
        self.sync_memo_state();
        // Another instance may have checked out drafts since we loaded.
        settings::keep_foreign_drafts(&mut self.memo_docs.borrow_mut(), &settings::memo_docs_on_disk());
        Settings {
            width: self.window.default_width(),
            height: self.window.default_height(),
            maximized: self.window.is_maximized(),
            hpaned: self.hpaned.position(),
            vpaned: self.lower.position(),
            files_paned: self.sidebar.position(),
            last_file: self.source.borrow().container().map(Path::to_path_buf),
            remote_images: self.remote_policy.get().as_str().to_string(),
            color_scheme: self.color_scheme.get().as_str().to_string(),
            theme: self.theme.get().as_str().to_string(),
            open_in_view: self.open_in_view.get(),
            open_last: self.open_last.get(),
            multiple_instances: self.multiple_instances.get(),
            show_attachment_note: self.show_attachment_note.get(),
            follow_cursor: self.follow_cursor.get(),
            keyword_highlight: self.keyword_highlight.get(),
            line_numbers: self.line_numbers.get(),
            preview_line_numbers: self.preview_line_numbers.get(),
            word_wrap: self.word_wrap.get(),
            editor_font: self.editor_font.get().as_str().to_string(),
            preview_font: self.preview_font.get().as_str().to_string(),
            preview_fit: self.preview_fit.get(),
            show_files: self.show_files.get(),
            show_insert: self.show_insert.get(),
            show_preview: self.show_preview.get(),
            horizontal: self.horizontal.get(),
            remember_folds: self.remember_folds.get(),
            doc_state: self.doc_state.borrow().clone(),
            recent_files: self.recent_files.borrow().clone(),
            frozen_files: self.frozen_files.borrow().clone(),
            memos_url: self.memos_url.borrow().clone(),
            memo_docs: self.memo_docs.borrow().clone(),
            memos_insecure: self.memos_insecure.get(),
        }
        .save();
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

/// Build the server handle for one request. Called on the worker thread:
/// reading the keyring is a blocking D-Bus call that can additionally wait
/// on an unlock prompt, so it must not happen on the main loop. Keeping it
/// here rather than in `App` also means the token is never held for longer
/// than the request that uses it.
fn memos_server(base: String, insecure: bool) -> Result<memos::Server, memos::Error> {
    if base.is_empty() {
        return Err(memos::Error::NoServer);
    }
    match secrets::load_token() {
        Some(token) => Ok(memos::Server { base, token, insecure }),
        None => Err(memos::Error::NoToken),
    }
}

/// Where memos are checked out. Under `~/.cache` on purpose: the copies are
/// re-fetchable, so it is right that a housekeeping sweep — ours or the
/// system's — may reclaim them.
fn memo_cache_dir() -> PathBuf {
    glib::user_cache_dir().join("bindle").join("memos")
}

/// Host part of a server URL, for the window subtitle of a checked-out memo.
fn server_host(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, rest)| rest);
    match host.trim_end_matches('/') {
        "" => "Memos".to_string(),
        host => host.to_string(),
    }
}

/// A folder as the title bar shows it, home abbreviated to `~`.
/// Component-wise, so /home/oyzmo2 is not "inside" /home/oy.
fn display_dir(dir: &Path) -> String {
    match dir.strip_prefix(glib::home_dir()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    }
}

/// File name of `path` for use as a link label / asset name.
fn file_label(path: &PathBuf) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string())
}

/// The printer's sheet as export geometry. GTK reports paper size already
/// oriented, so landscape needs no transform here. Its margins are the
/// printer's *unprintable* edges, which would put text at the very rim — so
/// they act as a floor and the A4 typographic margin wins whenever it is
/// larger, keeping print and PDF export visually identical on A4.
fn page_geometry(setup: &gtk4::PageSetup) -> export::Page {
    let unit = gtk4::Unit::Points;
    let hardware = [
        setup.top_margin(unit),
        setup.bottom_margin(unit),
        setup.left_margin(unit),
        setup.right_margin(unit),
    ]
    .into_iter()
    .fold(0.0f64, f64::max);
    export::Page {
        width: setup.paper_width(unit),
        height: setup.paper_height(unit),
        margin: hardware.max(export::Page::A4.margin),
    }
    .sane()
}

/// File stem of `path` for use as an exported document's title.
fn file_stem_label(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Document".to_string())
}

/// The Files panel's order with pinned files first, in the order they were
/// pinned (so a new pin joins the bottom of the group and the others stay
/// put); everything else keeps its place below them.
fn pinned_first(order: Vec<PathBuf>, frozen: &[PathBuf]) -> Vec<PathBuf> {
    let (mut pinned, rest): (Vec<PathBuf>, Vec<PathBuf>) =
        order.into_iter().partition(|p| frozen.contains(p));
    pinned.sort_by_key(|p| frozen.iter().position(|f| f == p));
    pinned.extend(rest);
    pinned
}

/// Copy `src` over `dest` without leaving `dest` half-written, like
/// `doc::write_atomic` (including its write-in-place fallback).
fn copy_atomic(src: &Path, dest: &Path) -> std::io::Result<()> {
    let copy = |file: &mut fs::File| std::io::copy(&mut fs::File::open(src)?, file).map(|_| ());
    match doc::replace_via_temp(dest, copy) {
        Ok(()) => Ok(()),
        Err(doc::Replace::Unsupported(_)) => fs::copy(src, dest).map(|_| ()),
        Err(doc::Replace::Failed(err)) => Err(err),
    }
}

/// See `App::asset_loc`.
enum AssetLoc {
    Bytes(Vec<u8>),
    Archive(PathBuf, bundle::PackInfo, String),
    File(PathBuf),
}

impl AssetLoc {
    /// The bytes, capped at `max`; None when missing or larger. Blocks.
    fn read(self, max: u64) -> Option<Vec<u8>> {
        match self {
            AssetLoc::Bytes(bytes) => (bytes.len() as u64 <= max).then_some(bytes),
            AssetLoc::Archive(path, info, rel) => bundle::read_asset(&path, &info, &rel, max),
            AssetLoc::File(path) => bundle::read_file_capped(&path, max),
        }
    }
}

/// Whether a relative reference climbs out of the folder it is resolved
/// against (or is not relative at all).
fn escapes_folder(rel: &Path) -> bool {
    use std::path::Component;
    let mut depth = 0i32;
    for part in rel.components() {
        match part {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

/// True for link destinations handled by URI schemes rather than files.
fn external_link(dest: &str) -> bool {
    dest.contains("://") || dest.starts_with("mailto:") || dest.starts_with("tel:")
}

/// A file name as markdown link text: brackets and backslashes escaped,
/// so `a]b.png` can't end the label early.
fn markdown_label(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A path as a markdown link destination. Plain when it is safe bare;
/// otherwise `<…>` (with `<`, `>` and `\` escaped), which is the form that
/// allows spaces and unbalanced parentheses (`a)b.png`).
fn markdown_dest(dest: &str) -> String {
    if !dest.chars().any(|c| matches!(c, ' ' | '(' | ')' | '<' | '>' | '\\')) {
        return dest.to_string();
    }
    let mut out = String::from("<");
    for c in dest.chars() {
        if matches!(c, '<' | '>' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('>');
    out
}

/// Whether gdk-pixbuf can load `path`, by extension alone — a clipboard
/// path may sit on a hung mount, so no stat or read on the main loop.
fn is_image_file(path: &Path) -> bool {
    let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_lowercase()) else {
        return false;
    };
    gtk4::gdk_pixbuf::Pixbuf::formats()
        .iter()
        .any(|format| format.extensions().iter().any(|e| e.as_str() == ext))
}

/// Express `target` relative to directory `base` (both absolute); falls back
/// to `target` unchanged when they share no common root.
fn relative_to(base: &Path, target: &Path) -> PathBuf {
    let base_parts: Vec<_> = base.components().collect();
    let target_parts: Vec<_> = target.components().collect();
    let common = base_parts
        .iter()
        .zip(&target_parts)
        .take_while(|(a, b)| *a == *b)
        .count();
    if common == 0 {
        return target.to_path_buf();
    }
    let mut out = PathBuf::new();
    for _ in common..base_parts.len() {
        out.push("..");
    }
    for part in &target_parts[common..] {
        out.push(part.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// A small dim heading above each sidebar panel, optionally followed by a
/// lighter hint (e.g. the panel's keyboard shortcut).
fn section(
    title: &str,
    hint: Option<&str>,
    child: &impl IsA<gtk4::Widget>,
) -> (gtk4::Box, gtk4::Label) {
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.set_margin_top(10);
    row.set_margin_start(14);
    let label = gtk4::Label::new(Some(title));
    label.add_css_class("heading");
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    row.append(&label);
    if let Some(hint) = hint {
        let hint = gtk4::Label::new(Some(hint));
        hint.add_css_class("caption");
        hint.add_css_class("dim-label");
        hint.set_valign(gtk4::Align::Baseline);
        row.append(&hint);
    }
    vbox.append(&row);
    vbox.append(child.as_ref());
    (vbox, label)
}

fn snippet_listbox() -> gtk4::ListBox {
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::None);
    list.add_css_class("navigation-sidebar");
    for snippet in snippets::all() {
        let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let icon = gtk4::Image::from_icon_name(snippet.icon);
        icon.add_css_class("dim-label");
        let label = gtk4::Label::new(Some(snippet.label));
        row_box.append(&icon);
        row_box.append(&label);
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&row_box));
        list.append(&row);
    }
    list
}

fn md_filters() -> gio::ListStore {
    let store = gio::ListStore::new::<gtk4::FileFilter>();
    let markdown = gtk4::FileFilter::new();
    markdown.set_name(Some("Supported files"));
    // Keep in step with `DocKind::of`; anything not listed still opens as
    // markdown through the "All files" filter.
    for pattern in [
        "*.md", "*.markdown", "*.mdown", "*.textpack", "*.odt", "*.txt", "*.text", "*.log", "*.csv",
        "*.tsv", "*.nfo", "*.diz", "*.json", "*.yaml", "*.yml", "*.toml", "*.ini", "*.cfg",
        "*.conf", "*.sh", "*.bash", "*.zsh", "*.ksh", "*.xml", "*.xsd", "*.xsl", "*.xslt",
    ] {
        markdown.add_pattern(pattern);
    }
    let all = gtk4::FileFilter::new();
    all.set_name(Some("All files"));
    all.add_pattern("*");
    store.append(&markdown);
    store.append(&all);
    store
}

#[cfg(test)]
mod tests {
    use super::{markdown_dest, markdown_label, pinned_first};
    use std::path::PathBuf;

    #[test]
    fn pinned_files_lead_in_pin_order_and_the_rest_keep_theirs() {
        let p = |names: &[&str]| names.iter().map(PathBuf::from).collect::<Vec<_>>();
        let order = p(&["a", "b", "c", "d", "e"]);
        // d pinned first, then b.
        assert_eq!(pinned_first(order, &p(&["d", "b"])), p(&["d", "b", "a", "c", "e"]));
        // Unpinning d: it heads the unpinned files rather than jumping down.
        assert_eq!(pinned_first(p(&["d", "b", "a", "c"]), &p(&["b"])), p(&["b", "d", "a", "c"]));
    }
    use pulldown_cmark::{Event, Parser, Tag};

    /// (link text, destination) of the first image `markdown` parses to.
    fn parsed(markdown: &str) -> Option<(String, String)> {
        let mut events = Parser::new(markdown);
        let dest = events.find_map(|e| match e {
            Event::Start(Tag::Image { dest_url, .. }) => Some(dest_url.to_string()),
            _ => None,
        })?;
        let text = events
            .take_while(|e| !matches!(e, Event::End(_)))
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect();
        Some((text, dest))
    }

    #[test]
    fn awkward_file_names_survive_as_image_references() {
        for (name, path) in [
            ("plain", "assets/plain.png"),
            ("a]b", "a)b.png"),
            ("x [1", "dir/x [1.png"),
            ("back\\slash", "odd<name>.png"),
        ] {
            let markdown = format!("![{}]({})", markdown_label(name), markdown_dest(path));
            assert_eq!(parsed(&markdown), Some((name.to_string(), path.to_string())), "{markdown}");
        }
        assert_eq!(markdown_dest("assets/plain.png"), "assets/plain.png");
    }
}
