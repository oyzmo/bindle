use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gtk4::prelude::*;
use gtk4::{gdk, gdk_pixbuf, gio, glib, pango};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Maximum thumbnail width in the preview pane.
const IMAGE_MAX_WIDTH: i32 = 220;

/// Refuse to decode remote images larger than this.
const REMOTE_MAX_BYTES: usize = 20 * 1024 * 1024;

enum Remote {
    Loading,
    Ready(gdk::Texture),
    Failed,
}

thread_local! {
    /// Downloaded remote images for this session, keyed by URL.
    /// Failed downloads stay Failed so they are not retried on every render.
    static REMOTE_CACHE: RefCell<HashMap<String, Remote>> = RefCell::new(HashMap::new());

    /// Scaled textures of local image files, keyed by path and invalidated
    /// by mtime — the preview re-renders on every debounced keystroke and
    /// re-decoding images each time is wasted work.
    static LOCAL_CACHE: RefCell<HashMap<PathBuf, (SystemTime, gdk::Texture)>> =
        RefCell::new(HashMap::new());

    /// Scaled textures of loader-provided assets (.textpack), keyed by
    /// destination and byte length. Stable texture identity also lets
    /// `splice` treat unchanged images as unchanged content.
    static LOADER_CACHE: RefCell<HashMap<(String, usize), gdk::Texture>> =
        RefCell::new(HashMap::new());

    /// Image scale for "Fit preview to width" (1.0 = thumbnails as decoded).
    static IMAGE_SCALE: std::cell::Cell<f64> = const { std::cell::Cell::new(1.0) };

    /// Shrunk copies of cached thumbnails at the current `IMAGE_SCALE`, keyed
    /// by the source texture's identity (kept alive in the value, so the key
    /// can't be reused). Stable identity keeps `splice` from re-inserting them.
    static FIT_CACHE: RefCell<HashMap<usize, (gdk::Texture, gdk::Texture)>> =
        RefCell::new(HashMap::new());
}

/// Set the factor images are shrunk by on the next render, matching the
/// fitted preview font. Changing it drops the shrunk copies of the old one;
/// returns whether it changed, i.e. whether images need a re-render.
pub fn set_image_scale(scale: f64) -> bool {
    let scale = scale.clamp(0.05, 1.0);
    let changed = IMAGE_SCALE.with(|s| s.replace(scale)) != scale;
    if changed {
        FIT_CACHE.with(|cache| cache.borrow_mut().clear());
    }
    changed
}

/// `texture` shrunk by `IMAGE_SCALE`, or itself at full scale.
fn fit_texture(texture: &gdk::Texture) -> gdk::Texture {
    let scale = IMAGE_SCALE.with(|s| s.get());
    if scale >= 1.0 {
        return texture.clone();
    }
    let key = texture.as_ptr() as usize;
    if let Some(fitted) = FIT_CACHE.with(|cache| cache.borrow().get(&key).map(|(_, f)| f.clone())) {
        return fitted;
    }
    let fitted = shrink_texture(texture, scale).unwrap_or_else(|| texture.clone());
    FIT_CACHE.with(|cache| cache.borrow_mut().insert(key, (texture.clone(), fitted.clone())));
    fitted
}

/// Downscale through cairo: `Texture::download` yields premultiplied BGRA,
/// which is cairo's ARGB32 on the little-endian machines this runs on.
fn shrink_texture(texture: &gdk::Texture, scale: f64) -> Option<gdk::Texture> {
    let (w, h) = (texture.width(), texture.height());
    let (tw, th) = (
        ((w as f64 * scale).round() as i32).max(1),
        ((h as f64 * scale).round() as i32).max(1),
    );
    let stride = cairo::Format::ARgb32.stride_for_width(w as u32).ok()?;
    let mut data = vec![0u8; stride as usize * h as usize];
    texture.download(&mut data, stride as usize);
    let src = cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, w, h, stride).ok()?;
    let mut dst = cairo::ImageSurface::create(cairo::Format::ARgb32, tw, th).ok()?;
    {
        let cr = cairo::Context::new(&dst).ok()?;
        cr.scale(tw as f64 / w as f64, th as f64 / h as f64);
        cr.set_source_surface(&src, 0.0, 0.0).ok()?;
        cr.source().set_filter(cairo::Filter::Good);
        cr.paint().ok()?;
    }
    dst.flush();
    let stride = dst.stride() as usize;
    let bytes = glib::Bytes::from(&*dst.data().ok()?);
    Some(
        gdk::MemoryTexture::new(tw, th, gdk::MemoryFormat::B8g8r8a8Premultiplied, &bytes, stride)
            .upcast(),
    )
}

/// Forget loader-provided textures. Call when the document changes: the key
/// is only (dest, length), so without this a same-named, same-sized asset of
/// another pack would be served the previous pack's image.
pub fn reset_loader_cache() {
    LOADER_CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Scale a pixbuf to the preview width and turn it into a texture.
fn scaled_texture(pixbuf: gdk_pixbuf::Pixbuf) -> Option<gdk::Texture> {
    let pixbuf = if pixbuf.width() > IMAGE_MAX_WIDTH {
        let height = (pixbuf.height() as f64 * IMAGE_MAX_WIDTH as f64 / pixbuf.width() as f64)
            .round()
            .max(1.0) as i32;
        pixbuf.scale_simple(IMAGE_MAX_WIDTH, height, gdk_pixbuf::InterpType::Bilinear)?
    } else {
        pixbuf
    };
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

fn texture_from_bytes(data: &[u8]) -> Option<gdk::Texture> {
    if data.len() > REMOTE_MAX_BYTES {
        return None;
    }
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(data));
    let pixbuf = gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    Some(scaled_texture(pixbuf)?)
}

/// Create the text tags used by the rendered preview. Call once per buffer.
///
/// The tinted ones (`code`, `quote`, `link`, `dim`) get their colours from
/// [`retint`] instead, so a theme change can restyle them in place.
pub fn setup_tags(buffer: &gtk4::TextBuffer) {
    let table = buffer.tag_table();
    let add = |tag: gtk4::TextTag| {
        table.add(&tag);
    };

    add(gtk4::TextTag::builder().name("h1").weight(800).scale(1.5).build());
    add(gtk4::TextTag::builder().name("h2").weight(700).scale(1.3).build());
    add(gtk4::TextTag::builder().name("h3").weight(700).scale(1.15).build());
    add(gtk4::TextTag::builder().name("h4").weight(600).scale(1.05).build());
    add(gtk4::TextTag::builder().name("bold").weight(700).build());
    add(gtk4::TextTag::builder().name("italic").style(pango::Style::Italic).build());
    add(gtk4::TextTag::builder().name("strike").strikethrough(true).build());
    add(gtk4::TextTag::builder()
        .name("code")
        .family("monospace")
        .build());
    add(gtk4::TextTag::builder()
        .name("quote")
        .style(pango::Style::Italic)
        .left_margin(16)
        .build());
    add(gtk4::TextTag::builder()
        .name("link")
        .underline(pango::Underline::Single)
        .build());
    // No soft wrapping: `emit_table` already wraps cells inside their
    // columns to the width it was given, and a soft wrap would break a row
    // mid-line and misalign every column after it. A table too wide to fit
    // even at `MIN_COL` per column scrolls horizontally instead.
    add(gtk4::TextTag::builder()
        .name("table")
        .family("monospace")
        .wrap_mode(gtk4::WrapMode::None)
        .build());
    add(gtk4::TextTag::builder().name("dim").build());
    // Verbatim rendering of non-markdown documents; the no-wrap variant is
    // for NFO art, which must keep exact line widths.
    add(gtk4::TextTag::builder().name("pre").family("monospace").build());
    add(gtk4::TextTag::builder()
        .name("pre-nowrap")
        .family("monospace")
        .wrap_mode(gtk4::WrapMode::None)
        .build());
    // Content of collapsed heading sections.
    add(gtk4::TextTag::builder().name("hidden").invisible(true).build());
}

/// Point the preview's tinted tags at `colors`.
///
/// Like the editor's `highlight::retint`, this only sets tag properties, so it
/// changes no text and no offsets — the splice diff in `ui.rs` compares
/// characters and tag *identity*, not tag colour, and is therefore unaffected.
pub fn retint(buffer: &gtk4::TextBuffer, colors: &crate::theme::Tags) {
    let table = buffer.tag_table();
    let set = |name: &str, color: &str| {
        if let Some(tag) = table.lookup(name) {
            tag.set_foreground(Some(color));
        }
    };
    set("quote", colors.dim);
    set("link", colors.link);
    set("dim", colors.dim);
    if let Some(tag) = table.lookup("code") {
        let (color, alpha) = colors.code_bg;
        let mut rgba: gtk4::gdk::RGBA = color.parse().unwrap_or(gtk4::gdk::RGBA::new(
            0.5, 0.5, 0.5, 1.0,
        ));
        rgba.set_alpha(alpha);
        tag.set_background_rgba(Some(&rgba));
    }
}

/// Maps character offsets in the rendered preview back to character offsets
/// in the markdown source, so a click in the preview can place the editor
/// cursor. Built during `render`; entries are in preview order.
#[derive(Default)]
pub struct SourceMap {
    entries: Vec<MapEntry>,
}

struct MapEntry {
    /// Start of this run in the preview buffer (chars).
    preview: i32,
    /// Length of the run in the preview buffer (chars).
    len: i32,
    /// Corresponding position in the markdown source (chars).
    source: i32,
    /// True when the run's text matches the source char for char, allowing
    /// exact positions inside the run instead of just its start.
    exact: bool,
}

impl SourceMap {
    /// Source char offset for a preview char offset, or None when the click
    /// landed before any mapped content.
    pub fn source_offset(&self, preview_offset: i32) -> Option<i32> {
        let i = self
            .entries
            .partition_point(|e| e.preview <= preview_offset)
            .checked_sub(1)?;
        let e = &self.entries[i];
        Some(if e.exact {
            e.source + (preview_offset - e.preview).min(e.len)
        } else {
            e.source
        })
    }

    /// Source char range (first, last char) behind preview chars `start..end`,
    /// from the runs that overlap it. Unlike `source_offset` this never
    /// borrows a preceding run's anchor, so an unmapped marker at the start
    /// of a line (a heading's `▾ `, a bullet) can't pull in the block above.
    pub fn source_span(&self, start: i32, end: i32) -> Option<(i32, i32)> {
        let at = |e: &MapEntry, p: i32| if e.exact { e.source + (p - e.preview) } else { e.source };
        let from = self.entries.partition_point(|e| e.preview + e.len <= start);
        self.entries[from..]
            .iter()
            .take_while(|e| e.preview < end)
            .filter(|e| e.len > 0)
            .map(|e| (at(e, start.max(e.preview)), at(e, end.min(e.preview + e.len) - 1)))
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    }

    /// Best-effort reverse lookup: the preview char offset showing the given
    /// source char offset. Source anchors are not monotonic in preview order,
    /// so this scans for the entry closest at-or-before the source position
    /// (preferring later entries on ties — the actual text over block gaps).
    pub fn preview_offset(&self, source_offset: i32) -> Option<i32> {
        let mut best: Option<(i32, i32)> = None; // (source gap, preview pos)
        for e in &self.entries {
            if e.source > source_offset {
                continue;
            }
            let gap = source_offset - e.source;
            let pos = if e.exact { e.preview + gap.min(e.len) } else { e.preview };
            match best {
                Some((bg, _)) if bg < gap => {}
                _ => best = Some((gap, pos)),
            }
        }
        best.map(|(_, pos)| pos)
            .or_else(|| self.entries.first().map(|e| e.preview))
    }
}

/// Incremental byte→char offset converter for the markdown source.
struct CharPos<'a> {
    text: &'a str,
    byte: usize,
    chars: i32,
}

impl CharPos<'_> {
    fn at(&mut self, byte: usize) -> i32 {
        let byte = byte.min(self.text.len());
        if byte < self.byte {
            self.byte = 0;
            self.chars = 0;
        }
        match self.text.get(self.byte..byte) {
            Some(s) => {
                self.chars += s.chars().count() as i32;
                self.byte = byte;
            }
            None => {} // not a char boundary; keep the previous position
        }
        self.chars
    }
}

/// One table cell plus the source anchor of its first text run.
#[derive(Default)]
struct TableCell {
    text: String,
    src: Option<(i32, bool)>,
}

/// Cell grid collected while parsing a table; row 0 is the header.
#[derive(Default)]
struct TableState {
    rows: Vec<Vec<TableCell>>,
    /// Source char offset where each row starts, parallel to `rows`.
    row_src: Vec<Option<i32>>,
}

struct Renderer<'a> {
    buffer: &'a gtk4::TextBuffer,
    /// Directory of the open document, for resolving relative image paths.
    base: Option<&'a Path>,
    /// When Some, local images are read through this instead of the
    /// filesystem (assets inside a .textpack).
    loader: Option<&'a dyn Fn(&str) -> Option<Vec<u8>>>,
    /// Called (later, from the main loop) when a remote image finishes
    /// downloading, so the owner can re-render the preview.
    refresh: Option<Rc<dyn Fn()>>,
    /// Whether fetching http(s) images is permitted for this render.
    allow_remote: bool,
    /// Set when the document references at least one http(s) image.
    saw_remote: bool,
    tags: Vec<&'static str>,
    /// One entry per open list; Some(n) for ordered lists, None for bullets.
    lists: Vec<Option<u64>>,
    /// Last two characters written, used to manage blank lines between blocks.
    tail: [char; 2],
    /// While Some, inline text is captured into cells instead of the buffer.
    table: Option<TableState>,
    /// True while inside an image whose thumbnail was inserted (drops alt text).
    in_rendered_image: bool,
    /// Source anchor (char offset, exact) of the event being rendered.
    map_src: Option<(i32, bool)>,
    map: SourceMap,
    /// Links currently open (start preview offset, destination).
    open_links: Vec<(i32, String)>,
    links: Vec<Link>,
    images: Vec<Image>,
    checkboxes: Vec<Checkbox>,
    /// Collapsible sections collected for the rendered headings.
    sections: Vec<Section>,
    /// Emitted table rows, for whole-row highlighting.
    table_rows: Vec<TableRow>,
    /// Characters of monospace that fit across the preview; tables wider
    /// than this wrap their cells. None lays them out at natural width.
    table_cols: Option<usize>,
    /// Occurrences per (level, text), keeping duplicate headings distinct.
    section_seen: HashMap<String, u32>,
    /// Marker offset and level of the heading currently being rendered.
    open_heading: Option<(i32, u8)>,
}

/// A collapsible heading section in the rendered preview. The marker is
/// the "▾ " glyph before the heading; while collapsed, ui.rs hides
/// [content_start, end) with the "hidden" tag (the text stays in the
/// buffer, so source-map and link offsets keep working).
#[derive(Clone)]
pub struct Section {
    /// Stable identity across re-renders: level, occurrence, heading text.
    pub key: String,
    pub level: u8,
    /// Preview char range of the disclosure marker.
    pub marker_start: i32,
    pub marker_end: i32,
    /// Preview char range hidden while collapsed: from after the heading
    /// line to the next heading of the same or a higher level.
    pub content_start: i32,
    pub end: i32,
}

/// A task list checkbox in the rendered preview. Clicking it edits the
/// markdown source (ui.rs), never the preview buffer.
#[derive(Clone, Copy)]
pub struct Checkbox {
    /// Preview char offset of the ☐/☑ glyph.
    pub start: i32,
    /// Source char offset of the `[` opening the marker.
    pub source: i32,
    pub checked: bool,
}

/// One line of a rendered table. Tables are re-laid out as padded columns,
/// so the character source map can't place a selection inside them; a
/// source row maps to its preview line as a whole instead. A row whose
/// cells wrap spans several preview lines, one entry each, all with the
/// same source.
#[derive(Clone, Copy)]
pub struct TableRow {
    /// Preview char range of the line, newline excluded.
    pub start: i32,
    pub end: i32,
    /// Source char offset of the row's start. The header's `─┼─` rule
    /// carries the header's offset with `separator` set: it stands for the
    /// `|---|` line right below the header. `None` when the row's start was
    /// not recorded — such a row never lights, rather than posing as line 0.
    pub source: Option<i32>,
    pub separator: bool,
}

/// A clickable link in the rendered preview.
#[derive(Clone)]
pub struct Link {
    /// Preview char range showing the link text.
    pub start: i32,
    pub end: i32,
    pub dest: String,
}

/// A thumbnail in the rendered preview: the one object-replacement char
/// holding it, and the image reference it shows (as written in the source).
#[derive(Clone)]
pub struct Image {
    pub offset: i32,
    pub dest: String,
}

impl<'a> Renderer<'a> {
    fn insert(&mut self, text: &str) {
        if text.is_empty() || self.in_rendered_image {
            return;
        }
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                if cell.src.is_none() {
                    cell.src = self.map_src;
                } else if !cell.text.is_empty() {
                    // Concatenated runs: the first run's exact anchor no
                    // longer maps the whole cell char-for-char.
                    cell.src = cell.src.map(|(start, _)| (start, false));
                }
                cell.text.push_str(text);
            }
            return;
        }
        let mut iter = self.buffer.end_iter();
        let at = iter.offset();
        if self.tags.is_empty() {
            self.buffer.insert(&mut iter, text);
        } else {
            self.buffer.insert_with_tags_by_name(&mut iter, text, &self.tags);
        }
        if let Some((source, exact)) = self.map_src {
            self.map.entries.push(MapEntry {
                preview: at,
                len: text.chars().count() as i32,
                source,
                exact,
            });
        }
        let mut chars = text.chars().rev();
        let last = chars.next().unwrap();
        self.tail = [chars.next().unwrap_or(self.tail[1]), last];
    }

    fn insert_dim(&mut self, text: &str) {
        self.tags.push("dim");
        self.insert(text);
        self.tags.pop();
    }

    /// Ensure the next block starts on its own line with a blank line above it.
    fn block_gap(&mut self) {
        if self.buffer.char_count() == 0 {
            return;
        }
        if self.tail[1] != '\n' {
            self.insert("\n");
        }
        if self.tail[0] != '\n' {
            self.insert("\n");
        }
    }

    fn line_break(&mut self) {
        if self.buffer.char_count() > 0 && self.tail[1] != '\n' {
            self.insert("\n");
        }
    }

    fn run(&mut self, markdown: &str) {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_TABLES);
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);

        let mut pos = CharPos { text: markdown, byte: 0, chars: 0 };
        // Anchor at the event's source start (or, for End events, its end so
        // trailing gaps map past the element). Text that survives parsing
        // verbatim gets an exact per-char mapping via `find`.
        let mut anchor = |range: &std::ops::Range<usize>, text: Option<&str>, at_end: bool| {
            if let Some(text) = text {
                if let Some(off) = markdown.get(range.clone()).and_then(|s| s.find(text)) {
                    return Some((pos.at(range.start + off), true));
                }
            }
            Some((pos.at(if at_end { range.end } else { range.start }), false))
        };

        for (event, range) in Parser::new_ext(markdown, options).into_offset_iter() {
            self.map_src = match &event {
                Event::Text(text) | Event::Code(text) => anchor(&range, Some(text), false),
                Event::End(_) => anchor(&range, None, true),
                _ => anchor(&range, None, false),
            };
            match event {
                Event::Start(tag) => self.start(tag),
                Event::End(tag) => self.end(tag),
                Event::Text(text) => self.insert(&text),
                Event::Code(text) => {
                    self.tags.push("code");
                    self.insert(&text);
                    self.tags.pop();
                }
                Event::SoftBreak => self.insert("\n"),
                Event::HardBreak => self.insert("\n"),
                Event::Rule => {
                    self.block_gap();
                    self.insert_dim("────────────────");
                }
                Event::TaskListMarker(done) => {
                    let start = self.buffer.end_iter().offset();
                    self.insert(if done { "☑ " } else { "☐ " });
                    // The marker's range starts at its `[`; ui.rs re-checks
                    // the bracket before editing, in case the text moved.
                    if let Some((source, _)) = self.map_src {
                        self.checkboxes.push(Checkbox { start, source, checked: done });
                    }
                }
                Event::Html(text) | Event::InlineHtml(text) => self.insert_dim(&text),
                _ => {}
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                // Inside list items the paragraph flows on the marker's line.
                if self.lists.is_empty() {
                    self.block_gap();
                }
            }
            Tag::Heading { level, .. } => {
                self.block_gap();
                // Disclosure marker; ui.rs toggles section folding on click.
                if self.table.is_none() {
                    let marker_start = self.buffer.end_iter().offset();
                    self.insert_dim("▾ ");
                    let n = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    self.open_heading = Some((marker_start, n));
                }
                self.tags.push(match level {
                    HeadingLevel::H1 => "h1",
                    HeadingLevel::H2 => "h2",
                    HeadingLevel::H3 => "h3",
                    _ => "h4",
                });
            }
            Tag::BlockQuote(_) => {
                self.block_gap();
                self.tags.push("quote");
            }
            Tag::CodeBlock(kind) => {
                self.block_gap();
                if let CodeBlockKind::Fenced(lang) = &kind {
                    if !lang.is_empty() {
                        self.insert_dim(&format!("{lang}\n"));
                    }
                }
                self.tags.push("code");
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.line_break();
                let indent = "    ".repeat(self.lists.len().saturating_sub(1));
                match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{indent}{n}. ");
                        *n += 1;
                        self.insert(&marker);
                    }
                    _ => self.insert(&format!("{indent}• ")),
                }
            }
            Tag::Emphasis => self.tags.push("italic"),
            Tag::Strong => self.tags.push("bold"),
            Tag::Strikethrough => self.tags.push("strike"),
            Tag::Link { dest_url, .. } => {
                self.tags.push("link");
                // Table cell text is buffered and emitted later, so offsets
                // recorded here would be wrong; links in tables stay inert.
                if self.table.is_none() {
                    self.open_links.push((self.buffer.end_iter().offset(), dest_url.to_string()));
                }
            }
            Tag::Image { dest_url, .. } => {
                // Table cells are buffered as text and emitted once the table
                // ends, so a thumbnail inserted now would land above the
                // table (and drop the cell's alt text). Cells get the
                // placeholder: an image can't sit in a monospace column anyway.
                let at = self.buffer.end_iter().offset();
                if self.table.is_none() && self.try_insert_image(&dest_url) {
                    self.in_rendered_image = true;
                    self.images.push(Image { offset: at, dest: dest_url.to_string() });
                } else {
                    self.insert("🖼 ");
                    self.tags.push("italic");
                }
            }
            Tag::Table(_) => {
                self.block_gap();
                self.table = Some(TableState::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                    table.row_src.push(self.map_src.map(|(s, _)| s));
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(TableCell::default());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.lists.is_empty() {
                    self.line_break();
                }
            }
            TagEnd::Heading(_) => {
                self.tags.pop();
                let Some((marker_start, level)) = self.open_heading.take() else {
                    self.line_break();
                    return;
                };
                let text_start = self.buffer.iter_at_offset(marker_start + 2);
                let text = self
                    .buffer
                    .text(&text_start, &self.buffer.end_iter(), false)
                    .to_string();
                self.line_break();
                let content_start = self.buffer.end_iter().offset();
                let seen = self.section_seen.entry(format!("{level}:{text}")).or_insert(0);
                let key = format!("{level}:{seen}:{text}");
                *seen += 1;
                self.sections.push(Section {
                    key,
                    level,
                    marker_start,
                    marker_end: marker_start + 2,
                    content_start,
                    end: 0,
                });
            }
            TagEnd::BlockQuote(_) | TagEnd::CodeBlock => {
                self.tags.pop();
                self.line_break();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.line_break();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.line_break();
            }
            TagEnd::Item => self.line_break(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.tags.pop();
            }
            TagEnd::Link => {
                self.tags.pop();
                if self.table.is_none() {
                    if let Some((start, dest)) = self.open_links.pop() {
                        let end = self.buffer.end_iter().offset();
                        if end > start {
                            self.links.push(Link { start, end, dest });
                        }
                    }
                }
            }
            TagEnd::Image => {
                if self.in_rendered_image {
                    self.in_rendered_image = false;
                } else {
                    self.tags.pop();
                }
            }
            _ => {}
        }
    }

    fn insert_texture(&mut self, texture: &gdk::Texture) {
        let mut iter = self.buffer.end_iter();
        let at = iter.offset();
        self.buffer.insert_paintable(&mut iter, &fit_texture(texture));
        self.tail = [self.tail[1], '\u{FFFC}'];
        if let Some((source, _)) = self.map_src {
            self.map.entries.push(MapEntry { preview: at, len: 1, source, exact: false });
        }
    }

    /// Insert a scaled thumbnail for a local file or a (cached) remote URL.
    /// Returns false when a placeholder should be shown instead — including
    /// while a remote image is still downloading; the refresh callback
    /// re-renders once it arrives.
    fn try_insert_image(&mut self, dest: &str) -> bool {
        if dest.starts_with("http://") || dest.starts_with("https://") {
            return self.try_insert_remote(dest);
        }
        if dest.contains("://") {
            return false;
        }
        let dest = crate::bundle::decode_dest(dest);
        if let Some(loader) = self.loader {
            let Some(bytes) = loader(&dest) else {
                return false;
            };
            let key = (dest.clone(), bytes.len());
            let cached = LOADER_CACHE.with(|cache| cache.borrow().get(&key).cloned());
            let texture = match cached {
                Some(texture) => texture,
                None => {
                    let Some(texture) = texture_from_bytes(&bytes) else {
                        return false;
                    };
                    LOADER_CACHE.with(|cache| {
                        cache.borrow_mut().insert(key, texture.clone());
                    });
                    texture
                }
            };
            self.insert_texture(&texture);
            return true;
        }
        let path = Path::new(&dest);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match self.base {
                Some(base) => base.join(path),
                None => return false,
            }
        };
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        if let Some(mtime) = mtime {
            let cached = LOCAL_CACHE.with(|cache| {
                cache
                    .borrow()
                    .get(&path)
                    .filter(|(stamp, _)| *stamp == mtime)
                    .map(|(_, texture)| texture.clone())
            });
            if let Some(texture) = cached {
                self.insert_texture(&texture);
                return true;
            }
        }
        let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(&path) else {
            return false;
        };
        let Some(texture) = scaled_texture(pixbuf) else {
            return false;
        };
        if let Some(mtime) = mtime {
            LOCAL_CACHE.with(|cache| {
                cache.borrow_mut().insert(path.clone(), (mtime, texture.clone()));
            });
        }
        self.insert_texture(&texture);
        true
    }

    fn try_insert_remote(&mut self, url: &str) -> bool {
        self.saw_remote = true;
        if !self.allow_remote {
            return false;
        }
        let cached = REMOTE_CACHE.with(|cache| match cache.borrow().get(url) {
            Some(Remote::Ready(texture)) => Some(Some(texture.clone())),
            Some(Remote::Loading) | Some(Remote::Failed) => Some(None),
            None => None,
        });
        match cached {
            Some(Some(texture)) => {
                self.insert_texture(&texture);
                return true;
            }
            Some(None) => return false,
            None => {}
        }
        // First sighting: fetch in the background via gio (handles https),
        // then re-render the preview through the refresh callback.
        REMOTE_CACHE.with(|cache| {
            cache.borrow_mut().insert(url.to_string(), Remote::Loading);
        });
        let key = url.to_string();
        let refresh = self.refresh.clone();
        gio::File::for_uri(url).load_contents_async(
            gio::Cancellable::NONE,
            move |result| {
                let entry = result
                    .ok()
                    .and_then(|(data, _etag)| texture_from_bytes(&data))
                    .map(Remote::Ready)
                    .unwrap_or(Remote::Failed);
                REMOTE_CACHE.with(|cache| {
                    cache.borrow_mut().insert(key, entry);
                });
                if let Some(refresh) = refresh {
                    refresh();
                }
            },
        );
        false
    }

    /// Emit a collected table as aligned monospace columns:
    ///   Column 1 │ Column 2
    ///   ─────────┼─────────
    ///   Cell     │ Cell
    /// When the natural widths overflow `table_cols`, columns shrink as in
    /// `fit_columns` and each cell wraps inside its own column, so a row
    /// grows taller instead of wider.
    fn emit_table(&mut self, table: TableState) {
        let cols = table.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let mut natural = vec![0usize; cols];
        let mut word = vec![0usize; cols];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                natural[i] = natural[i].max(cell.text.chars().count());
                let longest = cell.text.split(' ').map(|w| w.chars().count()).max().unwrap_or(0);
                word[i] = word[i].max(longest);
            }
        }
        let borders = 3 * (cols - 1);
        let widths = self
            .table_cols
            .and_then(|avail| fit_columns(&natural, &word, avail.checked_sub(borders)?))
            .unwrap_or_else(|| natural.clone());
        // Once rows wrap they would run into each other: a bordered blank
        // line between body rows keeps them apart.
        let spaced = widths != natural;
        self.tags.push("table");
        for (r, row) in table.rows.iter().enumerate() {
            let header = r == 0;
            let source = table.row_src.get(r).copied().flatten();
            let pieces: Vec<Vec<(usize, String)>> = (0..cols)
                .map(|i| row.get(i).map(|c| wrap_cell(&c.text, widths[i])).unwrap_or_default())
                .collect();
            let height = pieces.iter().map(Vec::len).max().unwrap_or(0).max(1);
            for line in 0..height {
                let row_start = self.buffer.end_iter().offset();
                for i in 0..cols {
                    let piece = pieces[i].get(line);
                    let text = piece.map(|(_, t)| t.as_str()).unwrap_or("");
                    // Cell text maps exactly to its source, shifted by where
                    // the piece starts in the cell; padding and borders
                    // anchor to the cell start.
                    let at = piece.map_or(0, |(at, _)| *at as i32);
                    self.map_src = row
                        .get(i)
                        .and_then(|c| c.src)
                        .map(|(s, exact)| if exact { (s + at, true) } else { (s, false) })
                        .or(self.map_src);
                    if header {
                        self.tags.push("bold");
                    }
                    self.insert(text);
                    if header {
                        self.tags.pop();
                    }
                    self.map_src = self.map_src.map(|(s, _)| (s, false));
                    if i + 1 < cols {
                        let pad = widths[i].saturating_sub(text.chars().count());
                        self.insert(&" ".repeat(pad));
                        self.insert_dim(" │ ");
                    }
                }
                let end = self.buffer.end_iter().offset();
                self.table_rows.push(TableRow { start: row_start, end, source, separator: false });
                self.insert("\n");
            }
            if spaced && !header && r + 1 < table.rows.len() {
                let mut line = String::new();
                for (i, width) in widths.iter().enumerate() {
                    if i + 1 < cols {
                        line.push_str(&" ".repeat(*width));
                        line.push_str(" │ ");
                    }
                }
                let start = self.buffer.end_iter().offset();
                self.insert_dim(line.trim_end());
                let end = self.buffer.end_iter().offset();
                self.table_rows.push(TableRow { start, end, source, separator: false });
                self.insert("\n");
            }
            if header {
                let mut line = String::new();
                for (i, width) in widths.iter().enumerate() {
                    line.push_str(&"─".repeat(*width));
                    if i + 1 < cols {
                        line.push_str("─┼─");
                    }
                }
                let start = self.buffer.end_iter().offset();
                self.insert_dim(&line);
                let end = self.buffer.end_iter().offset();
                self.table_rows.push(TableRow { start, end, source, separator: true });
                self.insert("\n");
            }
        }
        self.tags.pop();
    }
}

/// Narrowest a column is squeezed to (or its natural width, if less) when
/// even its longest word doesn't fit.
const MIN_COL: usize = 6;

/// Column widths (chars) for a table whose columns want `natural` and get
/// `avail` between them, borders excluded. Same rule as the PDF export's
/// `column_widths`: columns at or under their fair share keep their width,
/// the wider ones split what is left in proportion to how wide they want to
/// be. No column goes below its longest word (`word`) while those fit
/// side by side, so words only break when nothing else would do. None when
/// even `MIN_COL` per column would not fit — one letter per line is worse
/// than scrolling.
fn fit_columns(natural: &[usize], word: &[usize], avail: usize) -> Option<Vec<usize>> {
    if natural.iter().sum::<usize>() <= avail {
        return Some(natural.to_vec());
    }
    let n = natural.len();
    let mut floor: Vec<usize> =
        (0..n).map(|i| natural[i].min(word[i].max(MIN_COL))).collect();
    if floor.iter().sum::<usize>() > avail {
        floor = natural.iter().map(|w| (*w).min(MIN_COL)).collect();
        if floor.iter().sum::<usize>() > avail {
            return None;
        }
    }
    let mut fixed: Vec<Option<usize>> = vec![None; n];
    loop {
        let free: Vec<usize> = (0..n).filter(|i| fixed[*i].is_none()).collect();
        let used: usize = fixed.iter().flatten().sum();
        let room = avail.saturating_sub(used);
        if free.is_empty() {
            return Some(fixed.into_iter().map(|w| w.unwrap_or(0)).collect());
        }
        let share = room / free.len();
        if let Some(&i) = free.iter().find(|i| natural[**i] <= share) {
            fixed[i] = Some(natural[i]);
            continue;
        }
        let want: usize = free.iter().map(|i| natural[*i]).sum();
        let split = |i: usize| natural[i] * room / want;
        // A column whose split falls under its floor takes the floor, and
        // the others share what is left after it.
        if let Some(&i) = free.iter().find(|i| split(**i) < floor[**i]) {
            fixed[i] = Some(floor[i]);
            continue;
        }
        let mut widths: Vec<usize> = (0..n).map(|i| fixed[i].unwrap_or_else(|| split(i))).collect();
        // Hand the rounding remainder out a char at a time; every free
        // column is still under its natural width, so this terminates.
        let mut spare = avail.saturating_sub(widths.iter().sum());
        while spare > 0 {
            for &i in &free {
                if spare > 0 && widths[i] < natural[i] {
                    widths[i] += 1;
                    spare -= 1;
                }
            }
        }
        return Some(widths);
    }
}

/// Word-wrap a cell to `width` chars: (char offset in `text`, line) pairs.
/// Breaks at the last space that fits, hard-breaks words longer than the
/// column, and drops the spaces at a break.
fn wrap_cell(text: &str, width: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if width == 0 || chars.len() <= width {
        return vec![(0, text.to_string())];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        if !lines.is_empty() {
            while start < chars.len() && chars[start] == ' ' {
                start += 1;
            }
            if start == chars.len() {
                break;
            }
        }
        if chars.len() - start <= width {
            lines.push((start, chars[start..].iter().collect()));
            break;
        }
        let limit = start + width;
        let brk = (start + 1..=limit).rev().find(|&i| chars[i] == ' ').unwrap_or(limit);
        let mut end = brk;
        while end > start && chars[end - 1] == ' ' {
            end -= 1;
        }
        lines.push((start, chars[start..end].iter().collect()));
        start = brk;
    }
    lines
}

const OBJ: char = '\u{FFFC}';

fn paintable_at(buffer: &gtk4::TextBuffer, offset: i32) -> Option<gdk::Paintable> {
    buffer.iter_at_offset(offset).paintable()
}

/// Tags `ui.rs` puts on the live buffer *after* rendering, which therefore
/// never appear in a freshly rendered scratch buffer. They are excluded from
/// the diff below: counting them would make every collapsed section — or
/// every search hit — look changed on each render and re-splice the rest of
/// the document for nothing. Both are re-applied after the splice anyway.
const OVERLAY_TAGS: [&str; 4] = ["hidden", "find-match", "find-current", "line-pick"];

/// Spread a pointer across all 64 bits, so XORing tag identities together
/// yields a signature that distinguishes different sets of tags.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

/// A hash of the tags in effect at each character.
///
/// The diff has to compare formatting, not just characters: `###Two` losing
/// its missing space becomes a heading whose *text* ("Two") is unchanged, so
/// a text-only comparison leaves it in the common suffix and it keeps its
/// old paragraph tags. Built by walking tag toggles rather than querying
/// every character, so the cost tracks formatting density, not length.
fn tag_marks(buffer: &gtk4::TextBuffer, len: usize) -> Vec<u64> {
    // Resolve the overlay tags once and compare tags by identity below.
    // `tag.name()` allocates a GString, and this loop runs for every tag at
    // every toggle in the document on every render — doing it by name cost
    // about 5 ms per splice on a 40k-character document, which is felt as
    // typing latency. Buffers spliced together share a tag table, so
    // pointer identity is the same test as equal names.
    let table = buffer.tag_table();
    let overlay: Vec<gtk4::TextTag> =
        OVERLAY_TAGS.iter().filter_map(|name| table.lookup(name)).collect();

    let mut marks = vec![0u64; len];
    let mut iter = buffer.start_iter();
    // Running XOR of the tag identities in effect. XOR is its own inverse,
    // so a tag toggling on and later off cancels exactly — meaning only the
    // tags that *change* at each toggle need looking at. Rebuilding the
    // whole active set with `iter.tags()` instead was ~70 % of this
    // function's cost, because it walks and allocates the full list at
    // every toggle in the document.
    let mut signature = 0u64;
    loop {
        let start = iter.offset() as usize;
        if start >= len {
            break;
        }
        for toggled_on in [true, false] {
            for tag in iter.toggled_tags(toggled_on) {
                if !overlay.contains(&tag) {
                    signature ^= mix(tag.as_ptr() as u64);
                }
            }
        }
        let end = if iter.forward_to_tag_toggle(None::<&gtk4::TextTag>) {
            (iter.offset() as usize).min(len)
        } else {
            len
        };
        marks[start..end].fill(signature);
        if end >= len {
            break;
        }
    }
    marks
}

/// Make `target` equal to `rendered` by replacing only the differing middle
/// region. Unchanged lines keep their validated layout — a full `set_text`
/// invalidates every line and makes long documents visibly blink on each
/// re-render. Both buffers must share a tag table. Embedded images compare
/// by texture identity (the caches keep textures stable across renders).
pub fn splice(target: &gtk4::TextBuffer, rendered: &gtk4::TextBuffer) {
    let old: Vec<char> = target
        .slice(&target.start_iter(), &target.end_iter(), true)
        .chars()
        .collect();
    let new: Vec<char> = rendered
        .slice(&rendered.start_iter(), &rendered.end_iter(), true)
        .chars()
        .collect();

    // Characters alone are not enough: identical text whose formatting
    // changed must still be re-copied (see `tag_marks`).
    let old_tags = tag_marks(target, old.len());
    let new_tags = tag_marks(rendered, new.len());

    let mut prefix = old
        .iter()
        .zip(&new)
        .zip(old_tags.iter().zip(&new_tags))
        .take_while(|((a, b), (ta, tb))| a == b && ta == tb)
        .count();
    for i in 0..prefix {
        if old[i] == OBJ && paintable_at(target, i as i32) != paintable_at(rendered, i as i32) {
            prefix = i;
            break;
        }
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .zip(old_tags.iter().rev().zip(new_tags.iter().rev()))
        .take(max_suffix)
        .take_while(|((a, b), (ta, tb))| a == b && ta == tb)
        .count();
    for k in (1..=suffix).rev() {
        let (oi, ni) = (old.len() - k, new.len() - k);
        if old[oi] == OBJ && paintable_at(target, oi as i32) != paintable_at(rendered, ni as i32)
        {
            suffix = k - 1;
        }
    }

    let del_end = (old.len() - suffix) as i32;
    let ins_end = (new.len() - suffix) as i32;
    if prefix as i32 == del_end && prefix as i32 == ins_end {
        return;
    }
    if prefix as i32 != del_end {
        let mut start = target.iter_at_offset(prefix as i32);
        let mut end = target.iter_at_offset(del_end);
        target.delete(&mut start, &mut end);
    }
    copy_range(rendered, prefix as i32, ins_end, target, prefix as i32);
}

/// Copy `[from, to)` of `src` (text, tags, images) into `dst` at `at`.
fn copy_range(src: &gtk4::TextBuffer, from: i32, to: i32, dst: &gtk4::TextBuffer, at: i32) {
    let mut insert = dst.iter_at_offset(at);
    let mut pos = from;
    while pos < to {
        let iter = src.iter_at_offset(pos);
        if let Some(paintable) = iter.paintable() {
            dst.insert_paintable(&mut insert, &paintable);
            pos += 1;
            continue;
        }
        // A run of uniformly tagged text: up to the next tag toggle, the end
        // of the range, or the next embedded image.
        let mut run = iter.clone();
        run.forward_to_tag_toggle(None::<&gtk4::TextTag>);
        let mut run_end = run.offset().min(to).max(pos + 1);
        let mut slice = src.slice(&iter, &src.iter_at_offset(run_end), true).to_string();
        let cut = slice
            .chars()
            .enumerate()
            .find(|(k, c)| *k > 0 && *c == OBJ && paintable_at(src, pos + *k as i32).is_some())
            .map(|(k, _)| k);
        if let Some(k) = cut {
            run_end = pos + k as i32;
            slice = slice.chars().take(k).collect();
        }
        let tags = iter.tags();
        if tags.is_empty() {
            dst.insert(&mut insert, &slice);
        } else {
            let refs: Vec<&gtk4::TextTag> = tags.iter().collect();
            dst.insert_with_tags(&mut insert, &slice, &refs);
        }
        pos = run_end;
    }
}

/// Result of a preview render.
pub struct Rendered {
    /// True if the document references any http(s) image.
    pub saw_remote: bool,
    /// Maps preview positions back to markdown source positions.
    pub source_map: SourceMap,
    /// Clickable link ranges in the preview.
    pub links: Vec<Link>,
    /// Images shown as thumbnails, in document order.
    pub images: Vec<Image>,
    /// Task list checkboxes, in document order.
    pub checkboxes: Vec<Checkbox>,
    /// Collapsible heading sections, in document order.
    pub sections: Vec<Section>,
    /// Rendered table lines, in document order.
    pub table_rows: Vec<TableRow>,
}

/// Re-render `markdown` into the preview buffer. `base` is the directory of
/// the open document, used to resolve relative image paths. `refresh` is
/// invoked when a remote image finishes downloading and a re-render is due.
/// When `allow_remote` is false, http(s) images show placeholders and nothing
/// is fetched. `loader`, when given, resolves local image references instead
/// of the filesystem (assets inside a .textpack).
pub fn render(
    buffer: &gtk4::TextBuffer,
    markdown: &str,
    base: Option<&Path>,
    allow_remote: bool,
    refresh: Option<Rc<dyn Fn()>>,
    loader: Option<&dyn Fn(&str) -> Option<Vec<u8>>>,
) -> Rendered {
    render_fitted(buffer, markdown, base, allow_remote, refresh, loader, None)
}

/// [`render`], with tables fitted to `table_cols` monospace characters:
/// wider ones wrap their cells inside the columns.
pub fn render_fitted(
    buffer: &gtk4::TextBuffer,
    markdown: &str,
    base: Option<&Path>,
    allow_remote: bool,
    refresh: Option<Rc<dyn Fn()>>,
    loader: Option<&dyn Fn(&str) -> Option<Vec<u8>>>,
    table_cols: Option<usize>,
) -> Rendered {
    buffer.set_text("");
    let mut renderer = Renderer {
        buffer,
        base,
        loader,
        refresh,
        allow_remote,
        saw_remote: false,
        tags: Vec::new(),
        lists: Vec::new(),
        tail: ['\n', '\n'],
        table: None,
        in_rendered_image: false,
        map_src: None,
        map: SourceMap::default(),
        open_links: Vec::new(),
        links: Vec::new(),
        images: Vec::new(),
        checkboxes: Vec::new(),
        sections: Vec::new(),
        table_rows: Vec::new(),
        table_cols,
        section_seen: HashMap::new(),
        open_heading: None,
    };
    renderer.run(markdown);
    let mut sections = renderer.sections;
    let total = buffer.char_count();
    for i in 0..sections.len() {
        let end = sections[i + 1..]
            .iter()
            .find(|s| s.level <= sections[i].level)
            .map(|s| s.marker_start)
            .unwrap_or(total);
        sections[i].end = end;
    }
    Rendered {
        saw_remote: renderer.saw_remote,
        source_map: renderer.map,
        links: renderer.links,
        images: renderer.images,
        checkboxes: renderer.checkboxes,
        sections,
        table_rows: renderer.table_rows,
    }
}

/// Show a non-markdown document verbatim in monospace. `wrap` is false for
/// NFO files, whose ASCII art needs exact line widths (the preview scrolls
/// horizontally instead). The source map is an exact 1:1 mapping.
pub fn render_plain(buffer: &gtk4::TextBuffer, text: &str, wrap: bool) -> Rendered {
    buffer.set_text("");
    let mut iter = buffer.end_iter();
    let tag = if wrap { "pre" } else { "pre-nowrap" };
    buffer.insert_with_tags_by_name(&mut iter, text, &[tag]);
    let mut map = SourceMap::default();
    map.entries.push(MapEntry {
        preview: 0,
        len: text.chars().count() as i32,
        source: 0,
        exact: true,
    });
    Rendered {
        saw_remote: false,
        source_map: map,
        links: Vec::new(),
        images: Vec::new(),
        checkboxes: Vec::new(),
        sections: Vec::new(),
        table_rows: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_that_fit_keep_their_natural_widths() {
        assert_eq!(fit_columns(&[3, 10, 4], &[3, 5, 4], 17), Some(vec![3, 10, 4]));
    }

    #[test]
    fn narrow_columns_keep_their_width_and_wide_ones_share_the_rest() {
        let widths = fit_columns(&[3, 40, 20], &[3, 8, 8], 33).unwrap();
        assert_eq!(widths[0], 3);
        assert_eq!(widths.iter().sum::<usize>(), 33);
        assert!(widths[1] > widths[2]);
    }

    #[test]
    fn too_many_columns_for_the_width_fall_back_to_scrolling() {
        assert_eq!(fit_columns(&[20; 5], &[5; 5], 25), None);
    }

    #[test]
    fn columns_keep_their_longest_word_while_they_can() {
        // Proportional split would give the first column 5 and break
        // "Setting"; it keeps 7 and the long column gives way.
        assert_eq!(fit_columns(&[16, 80], &[7, 9], 30), Some(vec![7, 23]));
        // When the words can't all fit, squeeze to MIN_COL instead.
        assert_eq!(fit_columns(&[30, 30], &[20, 20], 14).map(|w| w.iter().sum::<usize>()), Some(14));
    }

    #[test]
    fn cells_wrap_at_spaces_and_hard_break_long_words() {
        assert_eq!(
            wrap_cell("the quick brown fox", 9),
            [(0, "the quick".to_string()), (10, "brown fox".to_string())]
        );
        assert_eq!(
            wrap_cell("abcdefghij kl", 4),
            [(0, "abcd".to_string()), (4, "efgh".to_string()), (8, "ij".to_string()), (11, "kl".to_string())]
        );
        assert_eq!(wrap_cell("short", 9), [(0, "short".to_string())]);
    }

    fn entry(preview: i32, len: i32, source: i32, exact: bool) -> MapEntry {
        MapEntry { preview, len, source, exact }
    }

    #[test]
    fn source_span_ignores_runs_outside_the_range() {
        // "para\n" at source 0, then a heading line "▾ Title\n" whose marker
        // is unmapped and whose text sits at source 20.
        let map = SourceMap {
            entries: vec![entry(0, 5, 0, true), entry(9, 5, 20, true)],
        };
        // The heading line is preview 7..14; its marker must not borrow "para".
        assert_eq!(map.source_span(7, 14), Some((20, 24)));
        assert_eq!(map.source_span(0, 4), Some((0, 3)));
        // A line with no mapped run has no span.
        assert_eq!(map.source_span(5, 7), None);
    }

    #[test]
    fn source_span_spans_several_runs_and_inexact_anchors() {
        let map = SourceMap {
            entries: vec![entry(0, 3, 10, false), entry(3, 4, 30, true)],
        };
        assert_eq!(map.source_span(0, 7), Some((10, 33)));
        assert_eq!(map.source_span(1, 2), Some((10, 10)));
    }
}
