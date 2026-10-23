use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use gtk4::prelude::*;
use gtk4::{gdk, gdk_pixbuf, gio, glib, pango};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Maximum thumbnail width in the preview pane.
const IMAGE_MAX_WIDTH: i32 = 220;

/// Refuse to download or decode remote images larger than this.
const REMOTE_MAX_BYTES: usize = 20 * 1024 * 1024;

/// Refuse to decode images with more pixels than this. A byte cap alone
/// bounds nothing: a 1 MB PNG can declare 40000×40000 and decode to 6 GB.
/// 120 MP still takes the largest common phone photos (108 MP).
const IMAGE_MAX_PIXELS: i64 = 120_000_000;

/// How often a local image is re-stat'ed for changes while the preview is
/// being re-rendered.
const LOCAL_RECHECK: Duration = Duration::from_secs(2);

enum Remote {
    Loading,
    Ready(gdk::Texture),
    Failed,
}

thread_local! {
    /// Downloaded remote images for this session, keyed by URL.
    /// Failed downloads stay Failed so they are not retried on every render.
    static REMOTE_CACHE: RefCell<HashMap<String, Remote>> = RefCell::new(HashMap::new());

    /// Thumbnails of local image files, keyed by path and invalidated by
    /// mtime — the preview re-renders on every debounced keystroke and
    /// re-decoding images each time is wasted work.
    static LOCAL_CACHE: RefCell<HashMap<PathBuf, LocalEntry>> = RefCell::new(HashMap::new());

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
    let gone: Vec<usize> = LOADER_CACHE.with(|cache| {
        cache.borrow_mut().drain().map(|(_, t)| t.as_ptr() as usize).collect()
    });
    forget_fitted(&gone);
}

/// Drop the shrunk copies of textures that are no longer cached, which
/// `FIT_CACHE` would otherwise keep alive (with their sources) until the
/// next scale change.
fn forget_fitted(sources: &[usize]) {
    FIT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        for key in sources {
            cache.remove(key);
        }
    });
}

/// A local image file's thumbnail. Stat'ing and decoding happen on a worker
/// thread: the file may sit on a network mount that hangs, and this runs on
/// every debounced keystroke.
struct LocalEntry {
    /// None while the first load runs, or when the file can't be shown.
    texture: Option<gdk::Texture>,
    mtime: Option<SystemTime>,
    /// When the file was last stat'ed.
    checked: Instant,
    /// A stat or load is running. Never start a second for the same path:
    /// on a hung mount they would pile up on the worker pool.
    busy: bool,
}

/// A decoded thumbnail's pixels: plain data, so it can come back from a
/// worker thread, which `Pixbuf` (not `Send`) can't.
struct Thumb {
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
    pixels: glib::Bytes,
}

impl Thumb {
    fn texture(&self) -> gdk::Texture {
        let format = if self.alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        gdk::MemoryTexture::new(self.width, self.height, format, &self.pixels, self.stride).upcast()
    }
}

/// Decode an image scaled to the preview width. The size is checked from
/// the header, before any pixels are decoded, so an image over
/// `IMAGE_MAX_PIXELS` costs nothing; scaling happens in the loader, which
/// lets JPEG decode at a reduced size outright.
fn decode_thumbnail(source: &mut dyn std::io::Read) -> Option<Thumb> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    let refused = Rc::new(std::cell::Cell::new(false));
    let flag = refused.clone();
    loader.connect_size_prepared(move |loader, w, h| {
        if w as i64 * h as i64 > IMAGE_MAX_PIXELS {
            flag.set(true);
        } else if w > IMAGE_MAX_WIDTH {
            let height = ((h as f64 * IMAGE_MAX_WIDTH as f64 / w as f64).round() as i32).max(1);
            loader.set_size(IMAGE_MAX_WIDTH, height);
        }
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let fed = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => loader.write(&buf[..n]).is_ok(),
            Err(_) => false,
        };
        // The loader must be closed whatever happens, or it warns when
        // finalized; the error from closing a half-fed one is expected.
        if !fed || refused.get() {
            let _ = loader.close();
            return None;
        }
    }
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    if pixbuf.bits_per_sample() != 8 || !matches!(pixbuf.n_channels(), 3 | 4) {
        return None;
    }
    Some(Thumb {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.n_channels() == 4,
        pixels: pixbuf.read_pixel_bytes(),
    })
}

fn texture_from_bytes(data: &[u8]) -> Option<gdk::Texture> {
    if data.len() > REMOTE_MAX_BYTES {
        return None;
    }
    decode_thumbnail(&mut &data[..]).map(|t| t.texture())
}

/// Stat (and, if it changed or is new, decode) a local image on a worker
/// thread; `known` is the mtime of the texture already shown, if any.
/// Re-renders through `refresh` only when what the preview shows changed.
fn load_local(path: PathBuf, known: Option<SystemTime>, refresh: Option<Rc<dyn Fn()>>) {
    glib::spawn_future_local(async move {
        let file = path.clone();
        let result = gio::spawn_blocking(move || {
            let mtime = fs::metadata(&file).ok().and_then(|m| m.modified().ok());
            if mtime.is_some() && mtime == known {
                return (mtime, None);
            }
            let thumb = fs::File::open(&file).ok().and_then(|mut f| decode_thumbnail(&mut f));
            (mtime, Some(thumb))
        })
        .await;
        let (mtime, loaded) = result.unwrap_or((None, Some(None)));
        let changed = LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            let Some(entry) = cache.get_mut(&path) else {
                return false;
            };
            entry.busy = false;
            entry.checked = Instant::now();
            // Unchanged on disk: keep what is shown.
            let Some(thumb) = loaded else {
                return false;
            };
            entry.mtime = mtime;
            let texture = thumb.map(|t| t.texture());
            let changed = entry.texture.is_some() || texture.is_some();
            if let Some(old) = std::mem::replace(&mut entry.texture, texture) {
                forget_fitted(&[old.as_ptr() as usize]);
            }
            changed
        });
        if changed {
            if let Some(refresh) = refresh {
                refresh();
            }
        }
    });
}

/// Read `url` into memory, giving up past `REMOTE_MAX_BYTES` rather than
/// buffering a response of any size first and checking afterwards.
async fn fetch_capped(url: &str) -> Option<Vec<u8>> {
    let stream = gio::File::for_uri(url).read_future(glib::Priority::DEFAULT).await.ok()?;
    let mut data = Vec::new();
    loop {
        let chunk = stream.read_bytes_future(64 * 1024, glib::Priority::DEFAULT).await.ok()?;
        if chunk.is_empty() {
            return Some(data);
        }
        if data.len() + chunk.len() > REMOTE_MAX_BYTES {
            return None;
        }
        data.extend_from_slice(&chunk);
    }
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
    loader: Option<&'a dyn Fn(&str) -> Option<glib::Bytes>>,
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
    /// Per open list item, spaces as wide as its marker: a further
    /// paragraph of the item lines up under the item's text.
    item_indents: Vec<String>,
    /// Nothing has been written since the innermost item's marker, so its
    /// first paragraph flows on the marker's line.
    item_fresh: bool,
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
        self.item_fresh = false;
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
                    // The box belongs with the marker: the paragraph after
                    // it is still the item's first.
                    let fresh = self.item_fresh;
                    self.insert(if done { "☑ " } else { "☐ " });
                    self.item_fresh = fresh;
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
                // Inside list items the first paragraph flows on the
                // marker's line. A further one (a loose item) is its own
                // block — without this it ran straight on: "• ab".
                if self.lists.is_empty() {
                    self.block_gap();
                } else if !self.item_fresh {
                    self.block_gap();
                    let indent = self.item_indents.last().cloned().unwrap_or_default();
                    self.insert(&indent);
                }
            }
            Tag::HtmlBlock => self.block_gap(),
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
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{indent}{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => format!("{indent}• "),
                };
                self.insert(&marker);
                self.item_indents.push(" ".repeat(marker.chars().count()));
                self.item_fresh = true;
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
            TagEnd::Item => {
                self.item_indents.pop();
                self.line_break();
            }
            TagEnd::HtmlBlock => self.line_break(),
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
        self.item_fresh = false;
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
        // Show whatever is cached and (re)check in the background: a first
        // sighting shows the placeholder until the load's refresh, a changed
        // file the old thumbnail until then.
        let (texture, check) = LOCAL_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.get_mut(&path) {
                Some(entry) => {
                    let due = !entry.busy && entry.checked.elapsed() >= LOCAL_RECHECK;
                    entry.busy |= due;
                    (entry.texture.clone(), due.then_some(entry.mtime))
                }
                None => {
                    let entry =
                        LocalEntry { texture: None, mtime: None, checked: Instant::now(), busy: true };
                    cache.insert(path.clone(), entry);
                    (None, Some(None))
                }
            }
        });
        if let Some(known) = check {
            load_local(path, known, self.refresh.clone());
        }
        match texture {
            Some(texture) => {
                self.insert_texture(&texture);
                true
            }
            None => false,
        }
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
        glib::spawn_future_local(async move {
            let thumb = match fetch_capped(&key).await {
                Some(data) => gio::spawn_blocking(move || decode_thumbnail(&mut &data[..]))
                    .await
                    .ok()
                    .flatten(),
                None => None,
            };
            let entry = thumb.map(|t| Remote::Ready(t.texture())).unwrap_or(Remote::Failed);
            REMOTE_CACHE.with(|cache| {
                cache.borrow_mut().insert(key, entry);
            });
            if let Some(refresh) = refresh {
                refresh();
            }
        });
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
                natural[i] = natural[i].max(text_width(&cell.text));
                let longest = cell.text.split(' ').map(text_width).max().unwrap_or(0);
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
                        let pad = widths[i].saturating_sub(text_width(text));
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

/// Monospace cells a char takes: two for East Asian wide/fullwidth text and
/// emoji-range pictographs, none for combining marks and zero-width chars,
/// one otherwise. Approximate (no grapheme clusters), but enough to keep CJK
/// text and the `🖼` placeholder from pushing their column out of line.
fn cell_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036F | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F000..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Monospace cells `text` takes; see [`cell_width`].
fn text_width(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// Word-wrap a cell to `width` monospace cells: (char offset in `text`,
/// line) pairs. Breaks at the last space that fits, hard-breaks words longer
/// than the column, and drops the spaces at a break.
fn wrap_cell(text: &str, width: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if width == 0 || text_width(text) <= width {
        return vec![(0, text.to_string())];
    }
    let cells: Vec<usize> = chars.iter().map(|c| cell_width(*c)).collect();
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
        if cells[start..].iter().sum::<usize>() <= width {
            lines.push((start, chars[start..].iter().collect()));
            break;
        }
        // The first char that no longer fits; at least one char per line,
        // even one wider than the column.
        let (mut limit, mut used) = (start, 0);
        while limit < chars.len() && used + cells[limit] <= width {
            used += cells[limit];
            limit += 1;
        }
        let limit = limit.max(start + 1);
        let brk = (start + 1..=limit).rev().find(|&i| chars.get(i) == Some(&' ')).unwrap_or(limit);
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
    loader: Option<&dyn Fn(&str) -> Option<glib::Bytes>>,
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
    loader: Option<&dyn Fn(&str) -> Option<glib::Bytes>>,
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
        item_indents: Vec::new(),
        item_fresh: false,
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

    #[test]
    fn wide_chars_count_two_cells() {
        assert_eq!(text_width("日本語"), 6);
        assert_eq!(text_width("🖼 cat"), 6);
        assert_eq!(text_width("e\u{301}"), 1);
        // Wraps by cells, not chars: two wide chars fill a 4-cell column.
        assert_eq!(
            wrap_cell("日本語です", 4),
            [(0, "日本".to_string()), (2, "語で".to_string()), (4, "す".to_string())]
        );
        // A char wider than the column still gets a line of its own.
        assert_eq!(wrap_cell("日本", 1), [(0, "日".to_string()), (1, "本".to_string())]);
    }

    #[test]
    fn oversized_images_are_refused_before_decoding() {
        // A PNG header declaring 20000×20000 (400 MP), with no pixel data:
        // the size check must stop it at the header.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        assert!(decode_thumbnail(&mut &png[..]).is_none());
        // A real image of ordinary size decodes, scaled to the preview width.
        let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 1000, 500).unwrap();
        let png = pixbuf.save_to_bufferv("png", &[]).unwrap();
        let thumb = decode_thumbnail(&mut &png[..]).expect("decodes");
        assert_eq!((thumb.width, thumb.height), (IMAGE_MAX_WIDTH, 110));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in data {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
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
