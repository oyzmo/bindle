//! Document export: self-contained single-file HTML (images embedded as
//! data: URIs) and paginated PDF rendered with pango on a cairo PDF surface.

use std::path::Path;

use gtk4::{gdk_pixbuf, gio, glib, pango};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// What the exported document contains. `Markdown` gets rendered structure;
/// `Plain` is emitted verbatim in monospace (TXT/NFO/JSON/YAML/TOML).
pub enum Content<'a> {
    Markdown {
        /// Resolves a local image reference to its bytes (filesystem,
        /// bundle assets or pack entries — provided by ui.rs).
        resolve: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    },
    Plain,
}

pub(crate) fn parser_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

pub(crate) fn is_local(dest: &str) -> bool {
    !dest.contains("://")
        && !dest.starts_with("mailto:")
        && !dest.starts_with("tel:")
        && !dest.starts_with('#')
        && !dest.starts_with("data:")
}

// --- HTML ---

const CSS: &str = "\
:root { color-scheme: light dark; }
body { font-family: 'Cantarell', 'Segoe UI', sans-serif; line-height: 1.6;
       max-width: 46rem; margin: 0 auto; padding: 2rem 1.2rem;
       color: #241f31; background: #ffffff; }
h1, h2, h3, h4 { line-height: 1.25; margin: 1.2em 0 0.5em; }
a { color: #1c71d8; }
code { font-family: monospace; font-size: 0.92em; background: #00000014;
       padding: 0.1em 0.35em; border-radius: 4px; }
pre { background: #00000010; padding: 0.8em 1em; border-radius: 8px;
      overflow-x: auto; }
pre code { background: none; padding: 0; }
pre.plain { background: none; padding: 0; }
blockquote { margin: 0.6em 0; padding-left: 1em;
             border-left: 3px solid #00000030; color: #5e5c64;
             font-style: italic; }
img { max-width: 100%; }
table { border-collapse: collapse; }
th, td { border: 1px solid #00000030; padding: 0.3em 0.7em; }
hr { border: none; border-top: 1px solid #00000030; margin: 1.5em 0; }
@media (prefers-color-scheme: dark) {
  body { color: #deddda; background: #242424; }
  a { color: #78aeed; }
  code, pre { background: #ffffff14; }
  pre.plain { background: none; }
  blockquote { border-color: #ffffff30; color: #949390; }
  th, td, hr { border-color: #ffffff30; }
}
";

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let v = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(v >> 18) as usize & 63] as char);
        out.push(ALPHABET[(v >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(v >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[v as usize & 63] as char } else { '=' });
    }
    out
}

/// Sniff an image MIME type from magic bytes; the bytes always came from an
/// image reference, so an image/* fallback beats octet-stream.
pub(crate) fn image_mime(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"\xFF\xD8") {
        "image/jpeg"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else if data.starts_with(b"<") || data.starts_with(b"\xEF\xBB\xBF<") {
        "image/svg+xml"
    } else {
        "image/png"
    }
}

fn data_uri(bytes: &[u8]) -> String {
    format!("data:{};base64,{}", image_mime(bytes), base64(bytes))
}

/// A complete standalone HTML document for `text`.
pub fn html_document(title: &str, text: &str, content: Content) -> String {
    let body = match content {
        Content::Markdown { resolve } => {
            let events = Parser::new_ext(text, parser_options()).map(|event| match event {
                Event::Start(Tag::Image { link_type, dest_url, title, id })
                    if is_local(&dest_url) =>
                {
                    let dest = match resolve(&dest_url) {
                        Some(bytes) => data_uri(&bytes).into(),
                        None => dest_url,
                    };
                    Event::Start(Tag::Image { link_type, dest_url: dest, title, id })
                }
                other => other,
            });
            let mut out = String::new();
            pulldown_cmark::html::push_html(&mut out, events);
            out
        }
        Content::Plain => format!("<pre class=\"plain\">{}</pre>", escape_html(text)),
    };
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>\n{}</style>\n</head>\n<body>\n{}</body>\n</html>\n",
        escape_html(title),
        CSS,
        body
    )
}

// --- PDF ---

/// Sheet geometry in PostScript points, the unit cairo's PDF surface works
/// in. Exporting always uses [`Page::A4`]; printing derives it from the paper
/// the user picked in the print dialog, so the spooled PDF is laid out for
/// the sheet it lands on rather than scaled onto it by the printer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub margin: f64,
}

impl Page {
    /// A4 with a typographic margin, the export default.
    pub const A4: Page = Page { width: 595.276, height: 841.89, margin: 56.0 };

    fn content_w(&self) -> f64 {
        self.width - 2.0 * self.margin
    }

    /// Last y a block may occupy.
    fn bottom(&self) -> f64 {
        self.height - self.margin
    }

    /// Clamp to something drawable: a printer whose hardware margins eat the
    /// sheet, or a page setup reported in a broken unit, must not yield a
    /// zero or negative content box (pango would panic on the width, and
    /// every block would "overflow" forever).
    pub fn sane(mut self) -> Page {
        self.width = self.width.max(72.0);
        self.height = self.height.max(72.0);
        let room = (self.width.min(self.height) - 72.0) / 2.0;
        self.margin = self.margin.clamp(0.0, room.max(0.0));
        self
    }
}

const BODY_FONT: &str = "Sans 10.5";
const CODE_FONT: &str = "Monospace 8.5";
/// Tables get their own (smaller, proportional) font: cell text wraps inside
/// its column, so it needs neither a fixed advance nor the body size.
const TABLE_FONT: &str = "Sans 9.5";
const TABLE_HEAD_FONT: &str = "Sans Bold 9.5";
/// Padding between a cell's text and its border.
const CELL_PAD: f64 = 5.0;
const HEADING_FONTS: [&str; 4] = ["Sans Bold 19", "Sans Bold 15", "Sans Bold 12.5", "Sans Bold 11"];

/// A table's plain cell text, first row the header. Column widths and line
/// wrapping are decided at draw time, where the page geometry is known.
struct TableData {
    rows: Vec<Vec<String>>,
    cols: usize,
}

/// One paginated unit: a text layout, an image, a table, or a horizontal
/// rule.
struct Block {
    text: String,
    /// `text` is pango markup (escaped, with inline style spans).
    markup: bool,
    font: &'static str,
    indent: f64,
    space_before: f64,
    image: Option<gdk_pixbuf::Pixbuf>,
    table: Option<TableData>,
    rule: bool,
}

impl Block {
    fn text(text: String, font: &'static str, indent: f64, space_before: f64) -> Block {
        Block {
            text,
            markup: true,
            font,
            indent,
            space_before,
            image: None,
            table: None,
            rule: false,
        }
    }

    fn table(table: TableData, indent: f64) -> Block {
        Block {
            text: String::new(),
            markup: false,
            font: TABLE_FONT,
            indent,
            space_before: 12.0,
            image: None,
            table: Some(table),
            rule: false,
        }
    }
}

fn escape_markup(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

pub(crate) fn pixbuf_from_bytes(bytes: &[u8]) -> Option<gdk_pixbuf::Pixbuf> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes));
    gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()
}

struct Builder<'a> {
    blocks: Vec<Block>,
    cur: String,
    font: &'static str,
    space: f64,
    indent: f64,
    /// (opener, closer) of the currently open inline tags, in open order.
    closers: Vec<(&'static str, &'static str)>,
    lists: Vec<Option<u64>>,
    quote: u32,
    table: Option<Vec<Vec<String>>>,
    /// Alt text of an image that got its own block is dropped.
    in_rendered_image: bool,
    resolve: &'a dyn Fn(&str) -> Option<Vec<u8>>,
}

impl Builder<'_> {
    fn flush(&mut self) {
        // Inline tags still open here — an image inside a link or emphasis
        // cuts the paragraph around its own block — are closed at the cut and
        // reopened after it. Every block's markup has to stand alone: pango
        // rejects an unbalanced one outright and its text never reaches the page.
        let reopen: String = self.closers.iter().map(|(open, _)| *open).collect();
        let text = std::mem::replace(&mut self.cur, reopen.clone());
        let mut trimmed = text.trim_end_matches('\n').to_string();
        if trimmed.is_empty() || trimmed == reopen {
            return;
        }
        for (_, close) in self.closers.iter().rev() {
            trimmed.push_str(close);
        }
        let body = if self.quote > 0 {
            format!("<span foreground=\"#77767b\" style=\"italic\">{trimmed}</span>")
        } else {
            trimmed
        };
        self.blocks.push(Block::text(body, self.font, self.indent, self.space));
        self.font = BODY_FONT;
        self.space = 8.0;
    }

    fn push_text(&mut self, text: &str) {
        if self.in_rendered_image {
            return;
        }
        if let Some(rows) = &mut self.table {
            if let Some(cell) = rows.last_mut().and_then(|r| r.last_mut()) {
                cell.push_str(text);
            }
            return;
        }
        self.cur.push_str(&escape_markup(text));
    }

    fn open(&mut self, opener: &'static str, closer: &'static str) {
        if self.table.is_none() && !self.in_rendered_image {
            self.cur.push_str(opener);
            self.closers.push((opener, closer));
        }
    }

    fn close(&mut self) {
        if self.table.is_none() && !self.in_rendered_image {
            if let Some((_, closer)) = self.closers.pop() {
                self.cur.push_str(closer);
            }
        }
    }

    fn run(&mut self, markdown: &str) {
        for event in Parser::new_ext(markdown, parser_options()) {
            match event {
                Event::Start(tag) => self.start(tag),
                Event::End(tag) => self.end(tag),
                Event::Text(text) => self.push_text(&text),
                Event::Code(text) => {
                    self.open("<tt>", "</tt>");
                    self.push_text(&text);
                    self.close();
                }
                Event::SoftBreak => self.push_text(" "),
                Event::HardBreak => self.push_text("\n"),
                Event::Rule => {
                    self.flush();
                    self.blocks.push(Block {
                        text: String::new(),
                        markup: false,
                        font: BODY_FONT,
                        indent: self.indent,
                        space_before: 10.0,
                        image: None,
                        table: None,
                        rule: true,
                    });
                }
                Event::TaskListMarker(done) => {
                    self.push_text(if done { "☑ " } else { "☐ " });
                }
                Event::Html(text) | Event::InlineHtml(text) => {
                    self.open("<span foreground=\"#77767b\">", "</span>");
                    self.push_text(&text);
                    self.close();
                }
                _ => {}
            }
        }
        self.flush();
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.flush();
                } else if !self.cur.is_empty() {
                    self.cur.push('\n');
                }
            }
            Tag::Heading { level, .. } => {
                self.flush();
                let i = (level as usize).clamp(1, 4) - 1;
                self.font = HEADING_FONTS[i];
                self.space = if i == 0 { 18.0 } else { 14.0 };
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.font = CODE_FONT;
                self.space = 10.0;
                if let CodeBlockKind::Fenced(lang) = &kind {
                    if !lang.is_empty() {
                        self.cur
                            .push_str(&format!("<span foreground=\"#77767b\">{}</span>\n", escape_markup(lang)));
                    }
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
                self.indent += 14.0;
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                self.indent = self.quote as f64 * 14.0
                    + 16.0 * self.lists.len().saturating_sub(1) as f64;
                self.space = 2.0;
                match self.lists.last_mut() {
                    Some(Some(n)) => {
                        self.cur.push_str(&format!("{n}. "));
                        *n += 1;
                    }
                    _ => self.cur.push_str("• "),
                }
            }
            Tag::Emphasis => self.open("<i>", "</i>"),
            Tag::Strong => self.open("<b>", "</b>"),
            Tag::Strikethrough => self.open("<s>", "</s>"),
            Tag::Link { .. } => {
                self.open("<span foreground=\"#1c71d8\" underline=\"single\">", "</span>")
            }
            Tag::Image { dest_url, .. } => {
                // A table's cells are emitted when it ends, so an image block
                // pushed now would land above the table: cells get the
                // placeholder and alt text, like the preview's.
                let pixbuf = if self.table.is_none() && is_local(&dest_url) {
                    (self.resolve)(&dest_url).and_then(|b| pixbuf_from_bytes(&b))
                } else {
                    None
                };
                match pixbuf {
                    Some(pixbuf) => {
                        self.flush();
                        self.blocks.push(Block {
                            text: String::new(),
                            markup: false,
                            font: BODY_FONT,
                            indent: self.indent,
                            space_before: 10.0,
                            image: Some(pixbuf),
                            table: None,
                            rule: false,
                        });
                        self.in_rendered_image = true;
                    }
                    // Unresolvable (remote or missing): placeholder + alt text.
                    None => self.push_text("🖼 "),
                }
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Vec::new());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(rows) = &mut self.table {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.last_mut()) {
                    row.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.lists.is_empty() {
                    self.flush();
                }
            }
            TagEnd::Heading(_) | TagEnd::CodeBlock | TagEnd::Item => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.indent = (self.indent - 14.0).max(0.0);
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.indent = self.quote as f64 * 14.0;
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.close()
            }
            TagEnd::Image => self.in_rendered_image = false,
            TagEnd::Table => {
                if let Some(rows) = self.table.take() {
                    self.emit_table(rows);
                }
            }
            _ => {}
        }
    }

    /// A real grid, drawn at export time. Monospace columns padded to the
    /// widest cell (what the preview does) fall apart here: one long cell
    /// pushes the row past the page width and pango wraps it mid-row, so
    /// every following column is misaligned.
    fn emit_table(&mut self, mut rows: Vec<Vec<String>>) {
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        // Ragged rows would leave holes in the grid.
        for row in &mut rows {
            row.resize(cols, String::new());
            for cell in row.iter_mut() {
                *cell = cell.trim().to_string();
            }
        }
        self.flush();
        self.blocks.push(Block::table(TableData { rows, cols }, self.indent));
    }
}

fn markdown_blocks(text: &str, resolve: &dyn Fn(&str) -> Option<Vec<u8>>) -> Vec<Block> {
    let mut builder = Builder {
        blocks: Vec::new(),
        cur: String::new(),
        font: BODY_FONT,
        space: 8.0,
        indent: 0.0,
        closers: Vec::new(),
        lists: Vec::new(),
        quote: 0,
        table: None,
        in_rendered_image: false,
        resolve,
    };
    builder.run(text);
    builder.blocks
}

/// The document as a list of blocks, ready to be paginated onto any page.
fn document_blocks(text: &str, content: Content) -> Vec<Block> {
    match content {
        Content::Markdown { resolve } => markdown_blocks(text, resolve),
        Content::Plain => vec![Block {
            text: text.to_string(),
            markup: false,
            font: "Monospace 8",
            indent: 0.0,
            space_before: 0.0,
            image: None,
            table: None,
            rule: false,
        }],
    }
}

/// Write `text` as a paginated A4 PDF to `path`.
pub fn write_pdf(path: &Path, title: &str, text: &str, content: Content) -> Result<(), String> {
    let blocks = document_blocks(text, content);
    let page = Page::A4;
    let surface =
        cairo::PdfSurface::new(page.width, page.height, path).map_err(|e| e.to_string())?;
    let _ = surface.set_metadata(cairo::PdfMetadata::Title, title);
    draw_document(&surface, &blocks, page)?;
    surface.finish();
    Ok(())
}

/// The same PDF in memory, laid out for `page`. This is what printing spools:
/// GTK's print dialog hands back an output stream that wants a PDF, so the
/// bytes never need a file — and the page comes from the chosen printer.
pub fn pdf_bytes(title: &str, text: &str, content: Content, page: Page) -> Result<Vec<u8>, String> {
    let blocks = document_blocks(text, content);
    let page = page.sane();
    let surface = cairo::PdfSurface::for_stream(page.width, page.height, Vec::<u8>::new())
        .map_err(|e| e.to_string())?;
    let _ = surface.set_metadata(cairo::PdfMetadata::Title, title);
    draw_document(&surface, &blocks, page)?;
    let stream = surface
        .finish_output_stream()
        .map_err(|e| e.to_string())?;
    stream
        .downcast::<Vec<u8>>()
        .map(|bytes| *bytes)
        .map_err(|_| "PDF buffer lost".to_string())
}

/// Paginate `blocks` onto `surface`, one `show_page` per sheet.
fn draw_document(
    surface: &cairo::PdfSurface,
    blocks: &[Block],
    page: Page,
) -> Result<(), String> {
    let cr = cairo::Context::new(surface).map_err(|e| e.to_string())?;
    let bottom = page.bottom();
    let mut y = page.margin;

    for block in blocks {
        if y > page.margin {
            y += block.space_before;
        }
        if block.rule {
            if y + 8.0 > bottom {
                cr.show_page().map_err(|e| e.to_string())?;
                y = page.margin;
            }
            cr.set_source_rgb(0.6, 0.6, 0.6);
            cr.set_line_width(0.7);
            cr.move_to(page.margin, y + 4.0);
            cr.line_to(page.width - page.margin, y + 4.0);
            cr.stroke().map_err(|e| e.to_string())?;
            y += 8.0;
            continue;
        }
        if let Some(pixbuf) = &block.image {
            y = draw_image(&cr, pixbuf, block.indent, y, page)?;
            continue;
        }
        if let Some(table) = &block.table {
            y = draw_table(&cr, table, block.indent, y, page)?;
            continue;
        }
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&pango::FontDescription::from_string(block.font)));
        layout.set_width(((page.content_w() - block.indent).max(1.0) * pango::SCALE as f64) as i32);
        layout.set_wrap(pango::WrapMode::WordChar);
        if block.markup {
            layout.set_markup(&block.text);
        } else {
            layout.set_text(&block.text);
        }
        let mut iter = layout.iter();
        loop {
            let (_, logical) = iter.line_extents();
            let scale = pango::SCALE as f64;
            let height = logical.height() as f64 / scale;
            let baseline = (iter.baseline() - logical.y()) as f64 / scale;
            if y + height > bottom && height < bottom - page.margin {
                cr.show_page().map_err(|e| e.to_string())?;
                y = page.margin;
            }
            cr.set_source_rgb(0.1, 0.1, 0.1);
            cr.move_to(page.margin + block.indent, y + baseline);
            if let Some(line) = iter.line_readonly() {
                pangocairo::functions::show_layout_line(&cr, &line);
            }
            y += height;
            if !iter.next_line() {
                break;
            }
        }
    }
    cr.show_page().map_err(|e| e.to_string())?;
    Ok(())
}

/// Lay out one row's cells, each wrapped inside its own column width.
fn cell_layouts(
    cr: &cairo::Context,
    row: &[String],
    widths: &[f64],
    header: bool,
) -> Vec<pango::Layout> {
    let font = pango::FontDescription::from_string(if header { TABLE_HEAD_FONT } else { TABLE_FONT });
    row.iter()
        .zip(widths)
        .map(|(cell, width)| {
            let layout = pangocairo::functions::create_layout(cr);
            layout.set_font_description(Some(&font));
            layout.set_width(((width - 2.0 * CELL_PAD).max(1.0) * pango::SCALE as f64) as i32);
            layout.set_wrap(pango::WrapMode::WordChar);
            layout.set_text(cell);
            layout
        })
        .collect()
}

fn row_height(layouts: &[pango::Layout]) -> f64 {
    layouts
        .iter()
        .map(|l| l.pixel_size().1 as f64)
        .fold(0.0, f64::max)
        + 2.0 * CELL_PAD
}

/// Column widths: each column's natural (unwrapped) width, shrunk to fit the
/// page when the total overflows. Only columns above their fair share give
/// space up, so a prose column wraps while `Default` / `Yes` columns keep
/// their words intact instead of every column being scaled down alike.
fn column_widths(cr: &cairo::Context, table: &TableData, avail: f64) -> Vec<f64> {
    let body = pango::FontDescription::from_string(TABLE_FONT);
    let head = pango::FontDescription::from_string(TABLE_HEAD_FONT);
    let mut natural = vec![0.0f64; table.cols];
    for (r, row) in table.rows.iter().enumerate() {
        for (i, cell) in row.iter().enumerate() {
            let layout = pangocairo::functions::create_layout(cr);
            layout.set_font_description(Some(if r == 0 { &head } else { &body }));
            layout.set_text(cell);
            natural[i] = natural[i].max(layout.pixel_size().0 as f64 + 2.0 * CELL_PAD);
        }
    }
    let total: f64 = natural.iter().sum();
    if total <= avail {
        return natural;
    }
    // Columns at or under their fair share of the page keep their natural
    // width; the rest split what is left in proportion to how wide they
    // want to be. Freezing a column can raise the share for the others, so
    // repeat until the set of frozen columns settles.
    let mut fixed = vec![false; table.cols];
    loop {
        let free: Vec<usize> = (0..table.cols).filter(|i| !fixed[*i]).collect();
        if free.is_empty() {
            return natural;
        }
        let used: f64 = natural
            .iter()
            .enumerate()
            .filter(|(i, _)| fixed[*i])
            .map(|(_, w)| *w)
            .sum();
        let room = avail - used;
        let share = room / free.len() as f64;
        match free.iter().find(|i| natural[**i] <= share) {
            Some(&i) => fixed[i] = true,
            None => {
                // The rest all want more than their share: split what is
                // left between them. The widths total `avail` exactly, so a
                // many-column table wraps hard rather than running off the
                // page — clipped text would be worse than cramped text.
                let want: f64 = free.iter().map(|i| natural[*i]).sum();
                let mut widths = natural.clone();
                for &i in &free {
                    widths[i] = natural[i] / want * room;
                }
                return widths;
            }
        }
    }
}

/// Draw one row: header shading, cell text, and the grid around each cell.
fn draw_row(
    cr: &cairo::Context,
    layouts: &[pango::Layout],
    widths: &[f64],
    x0: f64,
    y: f64,
    height: f64,
    header: bool,
) -> Result<(), String> {
    let mut x = x0;
    for (layout, width) in layouts.iter().zip(widths) {
        if header {
            cr.set_source_rgb(0.93, 0.93, 0.93);
            cr.rectangle(x, y, *width, height);
            cr.fill().map_err(|e| e.to_string())?;
        }
        cr.set_source_rgb(0.72, 0.72, 0.72);
        cr.set_line_width(0.5);
        cr.rectangle(x, y, *width, height);
        cr.stroke().map_err(|e| e.to_string())?;
        cr.set_source_rgb(0.1, 0.1, 0.1);
        cr.move_to(x + CELL_PAD, y + CELL_PAD);
        pangocairo::functions::show_layout(cr, layout);
        x += width;
    }
    Ok(())
}

/// Draw a table as a grid, breaking between rows and repeating the header at
/// the top of each new page. Returns the new y position.
fn draw_table(
    cr: &cairo::Context,
    table: &TableData,
    indent: f64,
    mut y: f64,
    page: Page,
) -> Result<f64, String> {
    let bottom = page.bottom();
    let x0 = page.margin + indent;
    let widths = column_widths(cr, table, (page.content_w() - indent).max(1.0));
    let header = table.rows.first().map(|row| {
        let layouts = cell_layouts(cr, row, &widths, true);
        let height = row_height(&layouts);
        (layouts, height)
    });

    for (r, row) in table.rows.iter().enumerate() {
        let is_header = r == 0;
        let (layouts, height) = if is_header {
            match &header {
                Some((layouts, height)) => (layouts.clone(), *height),
                None => continue,
            }
        } else {
            let layouts = cell_layouts(cr, row, &widths, false);
            let height = row_height(&layouts);
            (layouts, height)
        };
        // Break before a row that would run off the page — unless the page
        // is already empty, where a taller-than-a-page row has nowhere to go.
        if y + height > bottom && y > page.margin {
            cr.show_page().map_err(|e| e.to_string())?;
            y = page.margin;
            if !is_header {
                if let Some((head_layouts, head_height)) = &header {
                    draw_row(cr, head_layouts, &widths, x0, y, *head_height, true)?;
                    y += head_height;
                }
            }
        }
        draw_row(cr, &layouts, &widths, x0, y, height, is_header)?;
        y += height;
    }
    Ok(y)
}

/// Draw a pixbuf scaled to the content width via a PNG round-trip (the
/// portable path onto a PDF surface). Returns the new y position.
fn draw_image(
    cr: &cairo::Context,
    pixbuf: &gdk_pixbuf::Pixbuf,
    indent: f64,
    mut y: f64,
    page: Page,
) -> Result<f64, String> {
    let bottom = page.bottom();
    let png = pixbuf
        .save_to_bufferv("png", &[])
        .map_err(|e| e.to_string())?;
    let img = cairo::ImageSurface::create_from_png(&mut png.as_slice())
        .map_err(|e| e.to_string())?;
    // Pixels are 96 dpi; PDF points are 72 dpi.
    let natural_w = img.width() as f64 * 0.75;
    let natural_h = img.height() as f64 * 0.75;
    let available = (page.content_w() - indent).max(1.0);
    let mut scale = (available / natural_w).min(1.0);
    if natural_h * scale > bottom - page.margin {
        scale = (bottom - page.margin) / natural_h;
    }
    let height = natural_h * scale;
    if y + height > bottom {
        cr.show_page().map_err(|e| e.to_string())?;
        y = page.margin;
    }
    cr.save().map_err(|e| e.to_string())?;
    cr.translate(page.margin + indent, y);
    let s = scale * 0.75;
    cr.scale(s, s);
    cr.set_source_surface(&img, 0.0, 0.0).map_err(|e| e.to_string())?;
    cr.paint().map_err(|e| e.to_string())?;
    cr.restore().map_err(|e| e.to_string())?;
    Ok(y + height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_renders_structure_and_embeds_images() {
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 1, 2, 3];
        let resolve = |dest: &str| (dest == "pic.png").then(|| png.to_vec());
        let html = html_document(
            "Doc",
            "# Hi\n\n**bold** ![alt](pic.png) ![missing](gone.png)\n",
            Content::Markdown { resolve: &resolve },
        );
        assert!(html.contains("<title>Doc</title>"));
        assert!(html.contains("<h1>Hi</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("data:image/png;base64,iVBORwECAw=="));
        assert!(html.contains("src=\"gone.png\""));
    }

    #[test]
    fn html_plain_is_escaped_pre() {
        let html = html_document("T", "<not html> & stuff", Content::Plain);
        assert!(html.contains("<pre class=\"plain\">&lt;not html&gt; &amp; stuff</pre>"));
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    /// The PDF carries the rendered document, not its source: markers are
    /// consumed into fonts, bullets and markup spans. Only `Content::Plain`
    /// (TXT/NFO/JSON/YAML/TOML) is emitted verbatim.
    #[test]
    fn pdf_blocks_render_markdown_not_source() {
        let resolve = |_: &str| None;
        let blocks = markdown_blocks(
            "# Title\n\n## Sub\n\nA **bold** and *em* word.\n\n- one\n- two\n\n1. first\n\n\
             - [x] done\n\n| A | B |\n| - | - |\n| 1 | 2 |\n",
            &resolve,
        );
        let joined: String = blocks.iter().map(|b| b.text.as_str()).collect();
        // No heading hashes, emphasis markers or list dashes survive.
        assert!(!joined.contains('#'), "heading markers leaked: {joined}");
        assert!(!joined.contains("**"), "bold markers leaked: {joined}");
        assert!(!joined.contains("- "), "list markers leaked: {joined}");

        let title = &blocks[0];
        assert_eq!(title.text, "Title");
        assert_eq!(title.font, HEADING_FONTS[0]);
        assert_eq!(blocks[1].font, HEADING_FONTS[1]);
        assert!(blocks.iter().any(|b| b.text.contains("<b>bold</b>")));
        assert!(blocks.iter().any(|b| b.text.starts_with("• one")));
        assert!(blocks.iter().any(|b| b.text.starts_with("1. first")));
        assert!(blocks.iter().any(|b| b.text.contains("☑ done")));

        // Tables carry their cells for the grid drawn at export time.
        let table = blocks
            .iter()
            .find_map(|b| b.table.as_ref())
            .expect("table block");
        assert_eq!(table.cols, 2);
        assert_eq!(table.rows, vec![vec!["A", "B"], vec!["1", "2"]]);
    }

    fn tiny_png() -> Vec<u8> {
        gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 8, 8)
            .expect("pixbuf")
            .save_to_bufferv("png", &[])
            .expect("png")
    }

    /// An image inside a link or emphasis gets a block of its own, which
    /// cuts the paragraph's markup in two. Both halves must stay balanced:
    /// pango rejects unbalanced markup outright, and the text is then simply
    /// missing from the PDF — a linked badge used to swallow the sentence
    /// after it.
    #[test]
    fn an_image_inside_a_link_keeps_the_markup_around_it_balanced() {
        let png = tiny_png();
        let resolve = move |dest: &str| (dest == "badge.png").then(|| png.clone());
        let blocks = markdown_blocks(
            "Before [![b](badge.png) the link](https://x) after.\n\n*lead ![b](badge.png) tail*\n",
            &resolve,
        );
        for block in blocks.iter().filter(|b| b.markup) {
            assert!(
                pango::parse_markup(&block.text, '\0').is_ok(),
                "unbalanced markup: {:?}",
                block.text
            );
        }
        let joined: String = blocks.iter().map(|b| b.text.as_str()).collect();
        assert!(joined.contains("after."), "{joined}");
        assert!(joined.contains("tail"), "{joined}");
        assert_eq!(blocks.iter().filter(|b| b.image.is_some()).count(), 2);
    }

    /// Cells are collected as text and the grid is emitted when the table
    /// ends, so an image block pushed mid-table landed above the table and
    /// left its cell empty. Cells get the placeholder and alt text instead.
    #[test]
    fn images_in_table_cells_stay_in_their_cell() {
        let png = tiny_png();
        let resolve = move |dest: &str| (dest == "pic.png").then(|| png.clone());
        let blocks = markdown_blocks("| Pic | Name |\n| - | - |\n| ![cat](pic.png) | Tom |\n", &resolve);
        assert!(blocks.iter().all(|b| b.image.is_none()), "no image block may escape the table");
        let table = blocks.iter().find_map(|b| b.table.as_ref()).expect("table block");
        assert_eq!(table.rows[1], vec!["🖼 cat", "Tom"]);
    }

    #[test]
    fn table_rows_are_padded_and_trimmed() {
        let resolve = |_: &str| None;
        // A row short of cells would otherwise leave a hole in the grid.
        let blocks = markdown_blocks("| A | B | C |\n| - | - | - |\n| 1 | 2 | 3 |\n", &resolve);
        let table = blocks.iter().find_map(|b| b.table.as_ref()).expect("table block");
        assert_eq!(table.cols, 3);
        assert!(table.rows.iter().all(|r| r.len() == 3));
        assert_eq!(table.rows[1], vec!["1", "2", "3"]);
    }

    #[test]
    fn pdf_writes_a_file() {
        let path = std::env::temp_dir().join("bindle-export-test.pdf");
        let resolve = |_: &str| None;
        write_pdf(
            &path,
            "T",
            "# Title\n\nSome *text* over\n\n- a\n- b\n\n| A | B |\n| - | - |\n| 1 | 2 |\n",
            Content::Markdown { resolve: &resolve },
        )
        .expect("pdf written");
        let bytes = std::fs::read(&path).expect("file exists");
        assert!(bytes.starts_with(b"%PDF"));
        let _ = std::fs::remove_file(&path);
    }

    /// Printing renders to memory and must lay the document out for the
    /// sheet it was given — the printer is not asked to scale an A4 PDF.
    #[test]
    fn pdf_bytes_use_the_page_they_are_given() {
        let letter = Page { width: 612.0, height: 792.0, margin: 54.0 };
        let doc = "# Title\n\n".to_string() + &"A line of body text.\n\n".repeat(60);
        let render = |page| pdf_bytes("T", &doc, Content::Plain, page).expect("pdf");
        let bytes = render(letter);
        assert!(bytes.starts_with(b"%PDF"));
        // Cairo's output is deterministic, so a difference is the geometry
        // and nothing else (the page dictionary is inside a compressed
        // object stream, which is why this is checked by comparison).
        assert_eq!(bytes, render(letter));
        assert_ne!(bytes, render(Page::A4), "page size not applied");
        // Half the height paginates into more sheets, so pagination follows
        // the page too — not just the declared paper size.
        let short = Page { height: 400.0, ..letter };
        assert!(render(short).len() > bytes.len(), "pagination ignored the page height");
    }

    /// A printer whose margins swallow the sheet must still leave a content
    /// box: a zero or negative width would make every block overflow.
    #[test]
    fn insane_page_geometry_is_clamped() {
        let page = Page { width: 200.0, height: 300.0, margin: 500.0 }.sane();
        assert!(page.content_w() > 0.0);
        assert!(page.bottom() > page.margin);
        let tiny = Page { width: 0.0, height: -10.0, margin: -5.0 }.sane();
        assert!(tiny.content_w() > 0.0 && tiny.bottom() > tiny.margin);
        assert_eq!(Page::A4.sane(), Page::A4);
    }
}
