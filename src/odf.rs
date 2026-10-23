//! OpenDocument Text (.odt): import into markdown, export from it.
//!
//! Import is deliberately one-way. An .odt carries far more than markdown can
//! express — named styles, fonts, page layout, tracked changes, comments — so
//! writing an imported document back over it would silently destroy all of
//! that. `ui.rs` therefore never saves to an imported path: Save goes to Save
//! As. Embedded pictures stay in the package and are read lazily as
//! `Pictures/…` entries, the same way TextPack assets are.
//!
//! Both directions are approximate by design. Import keeps structure
//! (headings, paragraphs, emphasis, links, lists, tables, quotes, code,
//! images, footnotes) and drops presentation; export maps markdown onto the
//! standard LibreOffice style names, so the result edits naturally there.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use pulldown_cmark::{Alignment, Event, Parser, Tag, TagEnd};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event as Xml};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::export::{self, Content};

/// Cap on a decompressed XML part: enforced on the stream, since a hostile
/// zip header can understate the size.
const XML_MAX_BYTES: u64 = 50 * 1024 * 1024;

pub fn is_odt(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("odt"))
}

/// Read an .odt package and convert its body to markdown. Blocks on zip and
/// XML work, so it belongs on a worker thread like every other open.
pub fn import(path: &Path) -> io::Result<String> {
    let mut zip = ZipArchive::new(File::open(path)?).map_err(io::Error::other)?;
    let content = read_entry(&mut zip, "content.xml")?;
    // Common styles (Heading 1, Emphasis, list styles…) live here; a package
    // without it still imports, with fewer styles recognised.
    let styles = read_entry(&mut zip, "styles.xml").unwrap_or_default();
    to_markdown(&content, &styles).map_err(io::Error::other)
}

fn read_entry(zip: &mut ZipArchive<File>, name: &str) -> io::Result<String> {
    // Not `NotFound`: ui.rs reads that kind as "the share is not mounted".
    let entry = zip
        .by_name(name)
        .map_err(|_| io::Error::other(format!("no {name} in the package")))?;
    let mut text = String::new();
    entry.take(XML_MAX_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > XML_MAX_BYTES {
        return Err(io::Error::other(format!("{name} is unreasonably large")));
    }
    Ok(text)
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name.as_bytes())
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

// --- Styles ---

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Fmt {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}

/// Paragraph alignment, as far as a markdown table column can say it: left,
/// justified and unset all come out as an unaligned column.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
enum Align {
    #[default]
    None,
    Center,
    Right,
}

impl Align {
    fn parse(value: &str) -> Align {
        match value {
            "center" => Align::Center,
            "end" | "right" => Align::Right,
            _ => Align::None,
        }
    }
}

#[derive(Default)]
struct Style {
    parent: Option<String>,
    outline: Option<u8>,
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    font: Option<String>,
    align: Option<Align>,
}

/// What a style chain amounts to, nearest definition winning.
#[derive(Default)]
struct Resolved {
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    mono: Option<bool>,
    heading: Option<u8>,
    quote: bool,
    pre: bool,
    table_heading: bool,
    align: Option<Align>,
}

impl Resolved {
    fn apply(&self, base: Fmt) -> Fmt {
        Fmt {
            bold: self.bold.unwrap_or(base.bold),
            italic: self.italic.unwrap_or(base.italic),
            strike: self.strike.unwrap_or(base.strike),
            code: self.mono.unwrap_or(base.code),
        }
    }
}

#[derive(Default)]
struct Styles {
    /// Keyed "family/name": ODF names are unique per family only.
    styles: HashMap<String, Style>,
    /// List style name → whether each level (index 0 = level 1) is numbered.
    lists: HashMap<String, Vec<bool>>,
    mono_fonts: Vec<String>,
}

impl Styles {
    /// Collect style definitions from styles.xml or content.xml (automatic
    /// styles). Malformed XML just ends the scan with what was found.
    fn scan(&mut self, xml: &str) {
        let mut reader = Reader::from_str(xml);
        let mut current: Option<String> = None;
        let mut list: Option<String> = None;
        loop {
            let (e, empty) = match reader.read_event() {
                Ok(Xml::Start(e)) => (e, false),
                Ok(Xml::Empty(e)) => (e, true),
                Ok(Xml::End(e)) => {
                    match e.name().as_ref() {
                        b"style:style" => current = None,
                        b"text:list-style" => list = None,
                        _ => {}
                    }
                    continue;
                }
                Ok(Xml::Eof) | Err(_) => break,
                _ => continue,
            };
            match e.name().as_ref() {
                b"style:style" => {
                    let (Some(name), Some(family)) = (attr(&e, "style:name"), attr(&e, "style:family"))
                    else {
                        continue;
                    };
                    let key = format!("{family}/{name}");
                    let style = Style {
                        parent: attr(&e, "style:parent-style-name"),
                        outline: attr(&e, "style:default-outline-level").and_then(|v| v.parse().ok()),
                        ..Style::default()
                    };
                    self.styles.insert(key.clone(), style);
                    if !empty {
                        current = Some(key);
                    }
                }
                b"style:paragraph-properties" => {
                    let style = current.as_ref().and_then(|k| self.styles.get_mut(k));
                    if let (Some(s), Some(value)) = (style, attr(&e, "fo:text-align")) {
                        s.align = Some(Align::parse(&value));
                    }
                }
                b"style:text-properties" => {
                    let Some(s) = current.as_ref().and_then(|k| self.styles.get_mut(k)) else {
                        continue;
                    };
                    if let Some(w) = attr(&e, "fo:font-weight") {
                        s.bold = Some(w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600));
                    }
                    if let Some(v) = attr(&e, "fo:font-style") {
                        s.italic = Some(v == "italic" || v == "oblique");
                    }
                    if let Some(v) = attr(&e, "style:text-line-through-style") {
                        s.strike = Some(v != "none");
                    }
                    if let Some(v) = attr(&e, "style:font-name") {
                        s.font = Some(v);
                    }
                }
                b"text:list-style" if !empty => {
                    list = attr(&e, "style:name");
                    if let Some(name) = &list {
                        self.lists.entry(name.clone()).or_default();
                    }
                }
                b"text:list-level-style-number"
                | b"text:list-level-style-bullet"
                | b"text:list-level-style-image" => {
                    let Some(levels) = list.as_ref().and_then(|n| self.lists.get_mut(n)) else {
                        continue;
                    };
                    let level = attr(&e, "text:level")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .clamp(1, 10);
                    // An empty num-format is LibreOffice's "None" numbering.
                    let numbered = e.name().as_ref() == b"text:list-level-style-number"
                        && attr(&e, "style:num-format").is_none_or(|f| !f.is_empty());
                    if levels.len() < level {
                        levels.resize(level, false);
                    }
                    levels[level - 1] = numbered;
                }
                b"style:font-face" => {
                    let fixed = attr(&e, "style:font-pitch").as_deref() == Some("fixed")
                        || attr(&e, "style:font-family-generic").as_deref() == Some("modern");
                    if let (true, Some(name)) = (fixed, attr(&e, "style:name")) {
                        self.mono_fonts.push(name);
                    }
                }
                _ => {}
            }
        }
    }

    fn is_mono(&self, font: &str) -> bool {
        self.mono_fonts.iter().any(|f| f == font)
            || ["Mono", "Courier", "Consolas", "Code"].iter().any(|k| font.contains(k))
    }

    fn resolve(&self, family: &str, name: Option<&str>) -> Resolved {
        let mut r = Resolved::default();
        let mut cur = name.map(str::to_string);
        // Depth cap: a cyclic parent chain must not hang the import.
        for _ in 0..16 {
            let Some(name) = cur.take() else { break };
            let style = self.styles.get(&format!("{family}/{name}"));
            if let Some(s) = style {
                r.bold = r.bold.or(s.bold);
                r.italic = r.italic.or(s.italic);
                r.strike = r.strike.or(s.strike);
                if r.mono.is_none() {
                    r.mono = s.font.as_deref().map(|f| self.is_mono(f));
                }
                r.heading = r.heading.or(s.outline);
                r.align = r.align.or(s.align);
            }
            // Well-known names count whether or not the package defines them.
            match name.as_str() {
                "Preformatted_20_Text" => r.pre = true,
                "Quotations" => r.quote = true,
                "Title" => r.heading = r.heading.or(Some(1)),
                "Table_20_Heading" => r.table_heading = true,
                "Emphasis" => r.italic = r.italic.or(Some(true)),
                "Strong_20_Emphasis" => r.bold = r.bold.or(Some(true)),
                "Source_20_Text" | "Teletype" => r.mono = r.mono.or(Some(true)),
                other => {
                    if let Some(level) = other.strip_prefix("Heading_20_").and_then(|d| d.parse().ok()) {
                        r.heading = r.heading.or(Some(level));
                    }
                }
            }
            cur = style.and_then(|s| s.parent.clone());
        }
        r
    }
}

// --- Body ---

enum Inline {
    Text(Fmt, String),
    Link(String, Vec<Inline>),
    Image(String, String),
    Break,
    /// Already markdown (a footnote reference, a flattened nested table).
    Raw(String),
}

#[derive(Clone, Copy, PartialEq)]
enum ParaKind {
    Normal,
    Heading(u8),
    Quote,
    Pre,
}

struct Para {
    kind: ParaKind,
    /// Span formatting in effect; the bottom entry is the paragraph style's.
    fmts: Vec<Fmt>,
    links: Vec<(String, usize)>,
    runs: Vec<Inline>,
    /// ODF collapses whitespace like HTML; a paragraph starts collapsed.
    last_space: bool,
    align: Align,
}

impl Para {
    fn push(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let fmt = self.fmts.last().copied().unwrap_or_default();
        // Never merge across the start of an open link: its runs are split
        // off at that index when the link closes.
        let boundary = self.links.last().map_or(0, |(_, at)| *at);
        if let (true, Some(Inline::Text(last, text))) = (self.runs.len() > boundary, self.runs.last_mut()) {
            if *last == fmt {
                text.push_str(s);
                return;
            }
        }
        self.runs.push(Inline::Text(fmt, s.to_string()));
    }

    fn wrap_link(&mut self, href: String, at: usize) {
        let kids = self.runs.split_off(at.min(self.runs.len()));
        if href.is_empty() {
            self.runs.extend(kids);
        } else {
            self.runs.push(Inline::Link(href, kids));
        }
    }
}

enum Block {
    Text(String),
    Heading(u8, String),
    Quote(String),
    /// One line of a code block; consecutive lines share one fence.
    Code(String),
    Item { list: usize, indent: usize, marker: String, text: String },
    /// A later paragraph of a list item.
    ItemMore { indent: usize, text: String },
    Table(Vec<Vec<String>>, Vec<Align>),
}

#[derive(Clone, Copy)]
enum Container {
    Item { paras: usize, header: bool, start: Option<u64> },
    Cell,
    TextBox,
    Note,
}

/// Where a finished paragraph goes.
enum Sink {
    Body,
    Cell,
    TextBox,
    Note,
}

struct ListCtx {
    style: Option<String>,
    /// Top-level list identity: items of one list join without blank lines.
    id: usize,
    indent: usize,
    count: u64,
    marker_width: usize,
    numbered: bool,
}

#[derive(Default)]
struct TableCtx {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    repeat: usize,
    /// Per-cell alignment alongside `rows`; None for empty cells, which
    /// shouldn't outvote the ones with text.
    aligns: Vec<Vec<Option<Align>>>,
    row_aligns: Vec<Option<Align>>,
    cell_align: Option<Align>,
}

#[derive(Default)]
struct Frame {
    href: Option<String>,
    title: String,
    desc: String,
    /// 1 inside svg:title, 2 inside svg:desc.
    in_alt: u8,
}

/// Elements whose whole subtree is dropped: comments, deleted text of
/// tracked changes, generated indexes (a TOC's page numbers are noise in
/// markdown), cached list numbers, declarations and embedded objects.
const SKIPPED: &[&[u8]] = &[
    b"office:annotation",
    b"text:tracked-changes",
    b"text:table-of-content",
    b"text:alphabetical-index",
    b"text:illustration-index",
    b"text:table-index",
    b"text:object-index",
    b"text:user-index",
    b"text:bibliography",
    b"text:sequence-decls",
    b"text:variable-decls",
    b"text:user-field-decls",
    b"text:dde-connection-decls",
    b"text:note-citation",
    b"text:number",
    b"office:forms",
    b"draw:object",
    b"draw:object-ole",
];

struct Importer<'s> {
    styles: &'s Styles,
    blocks: Vec<Block>,
    paras: Vec<Para>,
    containers: Vec<Container>,
    lists: Vec<ListCtx>,
    /// Final count of the last top-level list per style, for lists that
    /// continue numbering.
    last_count: HashMap<String, u64>,
    tables: Vec<TableCtx>,
    frames: Vec<Frame>,
    notes: Vec<String>,
    note_text: Vec<String>,
    skip: usize,
    next_list_id: usize,
}

/// Convert content.xml (plus styles.xml for common styles) to markdown.
fn to_markdown(content: &str, styles_xml: &str) -> Result<String, String> {
    let mut styles = Styles::default();
    styles.scan(styles_xml);
    styles.scan(content);
    let mut imp = Importer {
        styles: &styles,
        blocks: Vec::new(),
        paras: Vec::new(),
        containers: Vec::new(),
        lists: Vec::new(),
        last_count: HashMap::new(),
        tables: Vec::new(),
        frames: Vec::new(),
        notes: Vec::new(),
        note_text: Vec::new(),
        skip: 0,
        next_list_id: 0,
    };
    let mut reader = Reader::from_str(content);
    let mut in_body = false;
    loop {
        let event = match reader.read_event() {
            Ok(Xml::Eof) => break,
            Ok(event) => event,
            Err(err) => return Err(format!("content.xml is malformed: {err}")),
        };
        match event {
            Xml::Start(e) if e.name().as_ref() == b"office:text" => in_body = true,
            Xml::End(e) if e.name().as_ref() == b"office:text" => in_body = false,
            _ if !in_body => {}
            Xml::Start(e) => imp.start(&e, false),
            Xml::Empty(e) => imp.start(&e, true),
            Xml::End(e) => imp.end(e.name().as_ref()),
            Xml::Text(t) => {
                if let Ok(s) = t.decode() {
                    imp.text(&s);
                }
            }
            Xml::CData(t) => {
                if let Ok(s) = t.decode() {
                    imp.text(&s);
                }
            }
            Xml::GeneralRef(r) => {
                let s = match r.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => match r.decode().as_deref() {
                        Ok("amp") => "&".into(),
                        Ok("lt") => "<".into(),
                        Ok("gt") => ">".into(),
                        Ok("quot") => "\"".into(),
                        Ok("apos") => "'".into(),
                        _ => String::new(),
                    },
                };
                imp.text(&s);
            }
            _ => {}
        }
    }
    Ok(imp.finish())
}

impl Importer<'_> {
    /// The nearest container a paragraph's text flows into; list items are
    /// transparent, since their paragraphs still become blocks.
    fn sink(&self) -> Sink {
        match self.containers.iter().rev().find(|c| !matches!(c, Container::Item { .. })) {
            Some(Container::Cell) => Sink::Cell,
            Some(Container::TextBox) => Sink::TextBox,
            Some(Container::Note) => Sink::Note,
            _ => Sink::Body,
        }
    }

    fn start(&mut self, e: &BytesStart, empty: bool) {
        if self.skip > 0 {
            if !empty {
                self.skip += 1;
            }
            return;
        }
        let qname = e.name();
        let name = qname.as_ref();
        if SKIPPED.contains(&name) {
            if !empty {
                self.skip = 1;
            }
            return;
        }
        match name {
            b"text:p" | b"text:h" => {
                self.open_para(e, name == b"text:h");
                if empty {
                    self.close_para();
                }
            }
            b"text:span" if !empty => {
                let r = self.styles.resolve("text", attr(e, "text:style-name").as_deref());
                if let Some(p) = self.paras.last_mut() {
                    let base = p.fmts.last().copied().unwrap_or_default();
                    p.fmts.push(r.apply(base));
                }
            }
            b"text:a" if !empty => {
                if let Some(p) = self.paras.last_mut() {
                    let href = clean_href(&attr(e, "xlink:href").unwrap_or_default());
                    p.links.push((href, p.runs.len()));
                }
            }
            b"text:s" => {
                let n = attr(e, "text:c").and_then(|v| v.parse().ok()).unwrap_or(1usize).min(1000);
                if let Some(p) = self.paras.last_mut() {
                    p.push(&" ".repeat(n));
                    p.last_space = true;
                }
            }
            b"text:tab" => {
                if let Some(p) = self.paras.last_mut() {
                    p.push(if p.kind == ParaKind::Pre { "\t" } else { " " });
                    p.last_space = true;
                }
            }
            b"text:line-break" => {
                if let Some(p) = self.paras.last_mut() {
                    p.runs.push(Inline::Break);
                    p.last_space = true;
                }
            }
            b"text:list" if !empty => self.open_list(e),
            b"text:list-item" | b"text:list-header" if !empty => {
                self.containers.push(Container::Item {
                    paras: 0,
                    header: name == b"text:list-header",
                    start: attr(e, "text:start-value").and_then(|v| v.parse().ok()),
                });
            }
            b"table:table" if !empty => self.tables.push(TableCtx::default()),
            b"table:table-cell" | b"table:covered-table-cell" => {
                // Capped: spreadsheet-style rows repeat an empty cell to the
                // edge of the sheet.
                let repeat = attr(e, "table:number-columns-repeated")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(1)
                    .clamp(1, 64);
                if let Some(t) = self.tables.last_mut() {
                    if empty {
                        t.row.extend(std::iter::repeat_n(String::new(), repeat));
                        t.row_aligns.extend(std::iter::repeat_n(None, repeat));
                    } else {
                        t.cell.clear();
                        t.cell_align = None;
                        t.repeat = repeat;
                    }
                }
                if !empty {
                    self.containers.push(Container::Cell);
                }
            }
            b"draw:frame" if !empty => self.frames.push(Frame::default()),
            b"draw:image" => {
                if let (Some(f), Some(href)) = (self.frames.last_mut(), attr(e, "xlink:href")) {
                    // An OLE object's replacement is a metafile no image
                    // loader can read.
                    if f.href.is_none() && !href.trim_start_matches("./").starts_with("ObjectReplacements")
                    {
                        f.href = Some(clean_href(&href));
                    }
                }
            }
            b"svg:title" if !empty => {
                if let Some(f) = self.frames.last_mut() {
                    f.in_alt = 1;
                }
            }
            b"svg:desc" if !empty => {
                if let Some(f) = self.frames.last_mut() {
                    f.in_alt = 2;
                }
            }
            b"draw:text-box" if !empty => self.containers.push(Container::TextBox),
            b"text:note-body" if !empty => {
                self.containers.push(Container::Note);
                self.note_text.push(String::new());
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &[u8]) {
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        match name {
            b"text:p" | b"text:h" => self.close_para(),
            b"text:span" => {
                if let Some(p) = self.paras.last_mut() {
                    if p.fmts.len() > 1 {
                        p.fmts.pop();
                    }
                }
            }
            b"text:a" => {
                if let Some(p) = self.paras.last_mut() {
                    if let Some((href, at)) = p.links.pop() {
                        p.wrap_link(href, at);
                    }
                }
            }
            b"text:list" => {
                if let Some(list) = self.lists.pop() {
                    if let (true, Some(style)) = (self.lists.is_empty(), list.style) {
                        self.last_count.insert(style, list.count);
                    }
                }
            }
            b"text:list-item" | b"text:list-header" | b"draw:text-box" => {
                self.containers.pop();
            }
            b"table:table-cell" | b"table:covered-table-cell" => {
                self.containers.pop();
                if let Some(t) = self.tables.last_mut() {
                    let cell = std::mem::take(&mut t.cell);
                    let align = t.cell_align.take().filter(|_| !cell.is_empty());
                    t.row.extend(std::iter::repeat_n(cell, t.repeat.max(1)));
                    t.row_aligns.extend(std::iter::repeat_n(align, t.repeat.max(1)));
                }
            }
            b"table:table-row" => {
                if let Some(t) = self.tables.last_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                    let aligns = std::mem::take(&mut t.row_aligns);
                    t.aligns.push(aligns);
                }
            }
            b"table:table" => self.close_table(),
            b"draw:frame" => self.close_frame(),
            b"svg:title" | b"svg:desc" => {
                if let Some(f) = self.frames.last_mut() {
                    f.in_alt = 0;
                }
            }
            b"text:note-body" => {
                self.containers.pop();
                let text = self.note_text.pop().unwrap_or_default();
                self.notes.push(text);
                let n = self.notes.len();
                if let Some(p) = self.paras.last_mut() {
                    p.runs.push(Inline::Raw(format!("[^{n}]")));
                    p.last_space = false;
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, s: &str) {
        if self.skip > 0 {
            return;
        }
        if let Some(f) = self.frames.last_mut() {
            match f.in_alt {
                1 => return f.title.push_str(s),
                2 => return f.desc.push_str(s),
                _ => {}
            }
        }
        let Some(p) = self.paras.last_mut() else { return };
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if matches!(c, ' ' | '\t' | '\n' | '\r') {
                if !p.last_space {
                    out.push(' ');
                    p.last_space = true;
                }
            } else {
                out.push(c);
                p.last_space = false;
            }
        }
        p.push(&out);
    }

    fn open_para(&mut self, e: &BytesStart, heading: bool) {
        let r = self.styles.resolve("paragraph", attr(e, "text:style-name").as_deref());
        let level = if heading {
            Some(attr(e, "text:outline-level").and_then(|v| v.parse().ok()).or(r.heading).unwrap_or(1))
        } else {
            r.heading
        };
        let kind = match level {
            Some(level) => ParaKind::Heading(level.clamp(1, 6)),
            None if r.pre => ParaKind::Pre,
            None if r.quote => ParaKind::Quote,
            None => ParaKind::Normal,
        };
        // Headings are bold by style and table headers by convention;
        // neither should come out wrapped in **.
        let base = if level.is_some() || r.table_heading {
            Fmt::default()
        } else {
            r.apply(Fmt::default())
        };
        self.paras.push(Para {
            kind,
            fmts: vec![base],
            links: Vec::new(),
            runs: Vec::new(),
            last_space: true,
            align: r.align.unwrap_or_default(),
        });
    }

    fn close_para(&mut self) {
        let Some(mut p) = self.paras.pop() else { return };
        // An unterminated link (malformed input) still keeps its text.
        while let Some((href, at)) = p.links.pop() {
            p.wrap_link(href, at);
        }
        match self.sink() {
            Sink::Body => self.emit_para(p),
            Sink::TextBox if !self.paras.is_empty() => {
                // A caption frame's text joins the paragraph it sits in.
                let outer = self.paras.last_mut().expect("checked");
                if !outer.runs.is_empty() && !p.runs.is_empty() {
                    outer.runs.push(Inline::Raw(" ".into()));
                }
                outer.runs.extend(p.runs);
                outer.last_space = true;
            }
            Sink::TextBox => self.emit_para(p),
            sink => {
                let cell = matches!(sink, Sink::Cell);
                let md = render_inline(&p.runs, if cell { Mode::Cell } else { Mode::Line });
                if cell && !md.is_empty() {
                    // A cell aligns like its first paragraph with text.
                    if let Some(t) = self.tables.last_mut() {
                        t.cell_align.get_or_insert(p.align);
                    }
                }
                self.sink_markdown(sink, md);
            }
        }
    }

    fn sink_markdown(&mut self, sink: Sink, md: String) {
        if md.is_empty() {
            return;
        }
        let join = |buf: &mut String| {
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(&md);
        };
        match sink {
            Sink::Cell => {
                if let Some(t) = self.tables.last_mut() {
                    join(&mut t.cell);
                }
            }
            Sink::Note => {
                if let Some(note) = self.note_text.last_mut() {
                    join(note);
                }
            }
            Sink::TextBox if !self.paras.is_empty() => {
                let p = self.paras.last_mut().expect("checked");
                p.runs.push(Inline::Raw(format!(" {md}")));
                p.last_space = false;
            }
            Sink::TextBox | Sink::Body => self.blocks.push(Block::Text(md)),
        }
    }

    fn emit_para(&mut self, p: Para) {
        let text = match p.kind {
            ParaKind::Pre => return self.blocks.push(Block::Code(raw_text(&p.runs))),
            ParaKind::Heading(level) => {
                let text = render_inline(&p.runs, Mode::Line);
                if !text.is_empty() {
                    self.blocks.push(Block::Heading(level, text));
                }
                return;
            }
            ParaKind::Quote => {
                let text = render_inline(&p.runs, Mode::Block);
                if !text.is_empty() {
                    self.blocks.push(Block::Quote(text));
                }
                return;
            }
            ParaKind::Normal => render_inline(&p.runs, Mode::Block),
        };
        if text.is_empty() {
            return;
        }
        let item = match self.containers.last_mut() {
            Some(Container::Item { paras, header, start }) => {
                let first = *paras == 0 && !*header;
                *paras += 1;
                Some((first, start.take()))
            }
            _ => None,
        };
        let (Some((first, start)), Some(list)) = (item, self.lists.last_mut()) else {
            return self.blocks.push(Block::Text(text));
        };
        if first {
            if let Some(start) = start {
                list.count = start.saturating_sub(1);
            }
            list.count += 1;
            let marker = if list.numbered { format!("{}.", list.count) } else { "-".to_string() };
            list.marker_width = marker.len() + 1;
            let pad = format!("\n{}", " ".repeat(list.indent + list.marker_width));
            self.blocks.push(Block::Item {
                list: list.id,
                indent: list.indent,
                marker,
                text: text.replace('\n', &pad),
            });
        } else {
            let indent = list.indent + list.marker_width.max(2);
            let pad = format!("\n{}", " ".repeat(indent));
            self.blocks.push(Block::ItemMore { indent, text: text.replace('\n', &pad) });
        }
    }

    fn open_list(&mut self, e: &BytesStart) {
        let style = attr(e, "text:style-name").or_else(|| self.lists.last().and_then(|l| l.style.clone()));
        let (id, indent) = match self.lists.last() {
            Some(parent) => (parent.id, parent.indent + parent.marker_width.max(2)),
            None => {
                self.next_list_id += 1;
                (self.next_list_id, 0)
            }
        };
        let continues = attr(e, "text:continue-numbering").as_deref() == Some("true")
            || attr(e, "text:continue-list").is_some();
        let count = match (&style, continues && self.lists.is_empty()) {
            (Some(style), true) => self.last_count.get(style).copied().unwrap_or(0),
            _ => 0,
        };
        let level = self.lists.len();
        let numbered = style
            .as_ref()
            .and_then(|s| self.styles.lists.get(s))
            .and_then(|levels| levels.get(level))
            .copied()
            .unwrap_or(false);
        self.lists.push(ListCtx { style, id, indent, count, marker_width: 0, numbered });
    }

    fn close_table(&mut self) {
        let Some(mut t) = self.tables.pop() else { return };
        if !t.row.is_empty() {
            t.rows.push(std::mem::take(&mut t.row));
            t.aligns.push(std::mem::take(&mut t.row_aligns));
        }
        let aligns = column_aligns(&t.aligns);
        let rows = trim_table(t.rows);
        if rows.is_empty() {
            return;
        }
        match self.sink() {
            Sink::Body => {
                let aligns = (0..rows[0].len()).map(|c| aligns.get(c).copied().unwrap_or_default()).collect();
                self.blocks.push(Block::Table(rows, aligns));
            }
            // Markdown tables don't nest: a table inside a cell, note or
            // text box flattens to its text.
            sink => {
                let flat: Vec<String> = rows
                    .iter()
                    .map(|r| r.iter().filter(|c| !c.is_empty()).cloned().collect::<Vec<_>>().join(", "))
                    .filter(|r| !r.is_empty())
                    .collect();
                self.sink_markdown(sink, flat.join("; "));
            }
        }
    }

    fn close_frame(&mut self) {
        let Some(f) = self.frames.pop() else { return };
        let Some(href) = f.href else { return };
        let alt = if f.title.trim().is_empty() { f.desc } else { f.title };
        let image = Inline::Image(href, alt.split_whitespace().collect::<Vec<_>>().join(" "));
        match self.paras.last_mut() {
            Some(p) => {
                p.runs.push(image);
                p.last_space = false;
            }
            None => {
                let sink = self.sink();
                self.sink_markdown(sink, render_inline(&[image], Mode::Line));
            }
        }
    }

    fn finish(mut self) -> String {
        while !self.paras.is_empty() {
            self.close_para();
        }
        let blocks = std::mem::take(&mut self.blocks);
        let mut out = String::new();
        let mut prev_list: Option<usize> = None;
        let mut i = 0;
        while i < blocks.len() {
            let list = match &blocks[i] {
                Block::Item { list, .. } => Some(*list),
                _ => None,
            };
            if i > 0 {
                out.push_str(if list.is_some() && list == prev_list { "\n" } else { "\n\n" });
            }
            prev_list = list;
            match &blocks[i] {
                Block::Code(_) => {
                    let mut lines = Vec::new();
                    while let Some(Block::Code(line)) = blocks.get(i) {
                        lines.push(line.as_str());
                        i += 1;
                    }
                    while lines.last().is_some_and(|l| l.trim().is_empty()) {
                        lines.pop();
                    }
                    let longest = lines.iter().map(|l| longest_run(l, '`')).max().unwrap_or(0);
                    let fence = "`".repeat(longest.max(2) + 1);
                    out.push_str(&format!("{fence}\n{}\n{fence}", lines.join("\n")));
                    continue;
                }
                Block::Quote(_) => {
                    let mut quoted = Vec::new();
                    while let Some(Block::Quote(text)) = blocks.get(i) {
                        quoted.push(text.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"));
                        i += 1;
                    }
                    out.push_str(&quoted.join("\n>\n"));
                    continue;
                }
                Block::Text(text) => out.push_str(text),
                Block::Heading(level, text) => {
                    out.push_str(&"#".repeat(*level as usize));
                    out.push(' ');
                    out.push_str(&escape_closing_hashes(text));
                }
                Block::Item { indent, marker, text, .. } => {
                    out.push_str(&format!("{}{marker} {text}", " ".repeat(*indent)));
                }
                Block::ItemMore { indent, text } => {
                    out.push_str(&format!("{}{text}", " ".repeat(*indent)));
                }
                Block::Table(rows, aligns) => out.push_str(&table_markdown(rows, aligns)),
            }
            i += 1;
        }
        for (n, note) in self.notes.iter().enumerate() {
            out.push_str(&format!("\n\n[^{}]: {note}", n + 1));
        }
        let mut out = out.trim_start().to_string();
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }
}

/// Package-relative hrefs, as a markdown file next to the .odt sees them.
fn clean_href(href: &str) -> String {
    let href = href.trim();
    let href = href.strip_prefix("./").unwrap_or(href);
    // Relative links resolve from *inside* the package, so LibreOffice
    // writes a file beside the .odt as "../name".
    if !href.contains("://") && !href.starts_with('#') {
        if let Some(rest) = href.strip_prefix("../") {
            return rest.to_string();
        }
    }
    href.to_string()
}

/// Drop trailing all-empty columns and rows; pad rows to one width.
fn trim_table(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let cols = (0..width)
        .rev()
        .find(|&c| rows.iter().any(|r| r.get(c).is_some_and(|s| !s.is_empty())))
        .map_or(0, |c| c + 1);
    if cols == 0 {
        return Vec::new();
    }
    for row in &mut rows {
        row.resize(cols, String::new());
    }
    while rows.last().is_some_and(|r| r.iter().all(String::is_empty)) {
        rows.pop();
    }
    rows
}

/// Each column's alignment: the most common among its cells with text, since
/// ODF aligns cell by cell. A tie leaves the column unaligned.
fn column_aligns(rows: &[Vec<Option<Align>>]) -> Vec<Align> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    (0..width)
        .map(|c| {
            let mut counts = [0usize; 3];
            for align in rows.iter().filter_map(|r| r.get(c).copied().flatten()) {
                counts[align as usize] += 1;
            }
            let top = counts.iter().copied().max().unwrap_or(0);
            let mut leaders = [Align::None, Align::Center, Align::Right]
                .into_iter()
                .filter(|align| counts[*align as usize] == top);
            match (leaders.next(), leaders.next()) {
                (Some(align), None) => align,
                // Shared first place (Center vs Right included): no majority.
                _ => Align::None,
            }
        })
        .collect()
}

fn table_markdown(rows: &[Vec<String>], aligns: &[Align]) -> String {
    let line = |row: &[String]| format!("| {} |", row.join(" | "));
    let rule: String = aligns
        .iter()
        .map(|align| match align {
            Align::None => " --- |",
            Align::Center => " :---: |",
            Align::Right => " ---: |",
        })
        .collect();
    let mut lines = vec![line(&rows[0]), format!("|{rule}")];
    lines.extend(rows[1..].iter().map(|r| line(r)));
    lines.join("\n")
}

fn longest_run(s: &str, ch: char) -> usize {
    let (mut best, mut cur) = (0, 0);
    for c in s.chars() {
        cur = if c == ch { cur + 1 } else { 0 };
        best = best.max(cur);
    }
    best
}

// --- Inline markdown ---

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// A paragraph: hard breaks allowed, line starts escaped.
    Block,
    /// One line (headings, link labels, notes).
    Line,
    /// A table cell: one line, pipes escaped.
    Cell,
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Bold,
    Italic,
    Strike,
}

impl Mark {
    fn token(self) -> &'static str {
        match self {
            Mark::Bold => "**",
            Mark::Italic => "*",
            Mark::Strike => "~~",
        }
    }
}

/// Emits formatted runs as markdown with a stack of open markers, closing
/// and opening only what changes between runs, and keeping whitespace
/// outside the markers (`** bold**` is not emphasis).
struct InlineWriter {
    out: String,
    open: Vec<Mark>,
    pending: String,
    brk: bool,
    mode: Mode,
}

fn render_inline(runs: &[Inline], mode: Mode) -> String {
    let mut w = InlineWriter { out: String::new(), open: Vec::new(), pending: String::new(), brk: false, mode };
    w.runs(runs);
    let closing: Vec<Mark> = w.open.drain(..).collect();
    for mark in closing.iter().rev() {
        w.out.push_str(mark.token());
    }
    let text = w.out.trim();
    if mode == Mode::Block {
        text.lines().map(escape_line_start).collect::<Vec<_>>().join("\n")
    } else {
        text.to_string()
    }
}

impl InlineWriter {
    fn runs(&mut self, runs: &[Inline]) {
        let mut i = 0;
        while i < runs.len() {
            match &runs[i] {
                Inline::Text(fmt, s) => {
                    let mut text = s.clone();
                    while let Some(Inline::Text(next, more)) = runs.get(i + 1) {
                        if next != fmt {
                            break;
                        }
                        text.push_str(more);
                        i += 1;
                    }
                    self.text(*fmt, &text);
                }
                Inline::Link(href, kids) => {
                    let mode = if self.mode == Mode::Cell { Mode::Cell } else { Mode::Line };
                    let inner = render_inline(kids, mode);
                    let label = if inner.is_empty() { escape(href, self.mode) } else { inner };
                    self.atom(&format!("[{label}]({})", dest(href)));
                }
                Inline::Image(src, alt) => {
                    self.atom(&format!("![{}]({})", escape(alt, self.mode), dest(src)));
                }
                Inline::Break if self.mode == Mode::Block => self.brk = true,
                Inline::Break => self.pending.push(' '),
                Inline::Raw(s) if s.trim().is_empty() => self.pending.push_str(s),
                Inline::Raw(s) => self.atom(s),
            }
            i += 1;
        }
    }

    fn text(&mut self, fmt: Fmt, s: &str) {
        let body = s.trim();
        if body.is_empty() {
            self.pending.push_str(s);
            return;
        }
        let lead = s.len() - s.trim_start().len();
        self.pending.push_str(&s[..lead]);
        let marks: Vec<Mark> = [(fmt.bold, Mark::Bold), (fmt.italic, Mark::Italic), (fmt.strike, Mark::Strike)]
            .into_iter()
            .filter_map(|(on, mark)| on.then_some(mark))
            .collect();
        self.transition(&marks);
        if fmt.code {
            self.out.push_str(&code_span(body, self.mode));
        } else {
            self.out.push_str(&escape(body, self.mode));
        }
        self.pending.push_str(&s[lead + body.len()..]);
    }

    /// Something that carries no emphasis of its own (link, image, note).
    fn atom(&mut self, md: &str) {
        self.transition(&[]);
        self.out.push_str(md);
    }

    fn transition(&mut self, marks: &[Mark]) {
        if let Some(pos) = self.open.iter().position(|m| !marks.contains(m)) {
            let closing: Vec<Mark> = self.open.drain(pos..).collect();
            for mark in closing.iter().rev() {
                self.out.push_str(mark.token());
            }
        }
        if self.brk {
            // A break with nothing before it would render as a stray "\".
            if !self.out.trim().is_empty() {
                self.out.push_str("\\\n");
            }
            self.brk = false;
            self.pending.clear();
        } else {
            self.out.push_str(&self.pending);
            self.pending.clear();
        }
        for mark in marks {
            if !self.open.contains(mark) {
                self.out.push_str(mark.token());
                self.open.push(*mark);
            }
        }
    }
}

fn escape(s: &str, mode: Mode) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        let needs = match c {
            '\\' | '*' | '`' | '[' | ']' | '<' | '~' => true,
            // Intraword underscores (snake_case) cannot start emphasis.
            '_' => !(i > 0
                && chars[i - 1].is_alphanumeric()
                && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric())),
            // Only where it could read as an entity (&amp; &#169;).
            '&' => chars.get(i + 1).is_some_and(|n| n.is_alphanumeric() || *n == '#'),
            '|' => mode == Mode::Cell,
            _ => false,
        };
        if needs {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Heading text with a trailing `#` run escaped. Markdown reads ` #…` at the
/// end of an ATX heading as an optional closing sequence and drops it, so
/// "Issue #" would come back as "Issue"; "C#" (no space before) is safe.
fn escape_closing_hashes(text: &str) -> String {
    let head = text.trim_end_matches('#');
    if head.len() < text.len() && (head.is_empty() || head.ends_with(' ')) {
        format!("{head}\\{}", &text[head.len()..])
    } else {
        text.to_string()
    }
}

/// Text that would start a heading, quote, list, rule or setext underline.
fn escape_line_start(line: &str) -> String {
    if line.starts_with(['#', '>', '-', '+', '=']) {
        return format!("\\{line}");
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &line[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") || rest == "." || rest == ")" {
            return format!("{}\\{rest}", &line[..digits]);
        }
    }
    line.to_string()
}

fn code_span(s: &str, mode: Mode) -> String {
    // GFM splits cells on pipes even inside code spans.
    let s = if mode == Mode::Cell { s.replace('|', "\\|") } else { s.to_string() };
    let fence = "`".repeat(longest_run(&s, '`') + 1);
    let pad = if s.starts_with('`') || s.ends_with('`') { " " } else { "" };
    format!("{fence}{pad}{s}{pad}{fence}")
}

fn dest(href: &str) -> String {
    if href.contains([' ', '(', ')', '<', '>']) {
        format!("<{}>", href.replace('<', "%3C").replace('>', "%3E"))
    } else {
        href.to_string()
    }
}

fn raw_text(runs: &[Inline]) -> String {
    runs.iter()
        .map(|run| match run {
            Inline::Text(_, s) | Inline::Raw(s) => s.clone(),
            Inline::Link(_, kids) => raw_text(kids),
            Inline::Image(..) => String::new(),
            Inline::Break => "\n".to_string(),
        })
        .collect()
}

// --- Export ---

/// Text area of the A4 page styles.xml declares (2 cm margins); pictures
/// shrink to fit it.
const TEXT_WIDTH_CM: f64 = 17.0;
const TEXT_HEIGHT_CM: f64 = 25.0;

const NAMESPACES: &str = "xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
    xmlns:style=\"urn:oasis:names:tc:opendocument:xmlns:style:1.0\" \
    xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" \
    xmlns:table=\"urn:oasis:names:tc:opendocument:xmlns:table:1.0\" \
    xmlns:draw=\"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0\" \
    xmlns:fo=\"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0\" \
    xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
    xmlns:svg=\"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0\" \
    xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
    xmlns:meta=\"urn:oasis:names:tc:opendocument:xmlns:meta:1.0\"";

/// Common styles, under LibreOffice's own names so the document edits like
/// one it made (and so `Styles::resolve` recognises them on re-import).
const STYLES_BODY: &str = r##"<office:font-face-decls>
<style:font-face style:name="Liberation Mono" svg:font-family="'Liberation Mono'" style:font-family-generic="modern" style:font-pitch="fixed"/>
</office:font-face-decls>
<office:styles>
<style:default-style style:family="paragraph"><style:paragraph-properties fo:orphans="2" fo:widows="2"/><style:text-properties fo:font-size="11pt"/></style:default-style>
<style:style style:name="Standard" style:family="paragraph" style:class="text"/>
<style:style style:name="Text_20_body" style:display-name="Text body" style:family="paragraph" style:parent-style-name="Standard" style:class="text"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0.247cm" fo:line-height="115%"/></style:style>
<style:style style:name="Heading" style:family="paragraph" style:parent-style-name="Standard" style:next-style-name="Text_20_body" style:class="text"><style:paragraph-properties fo:margin-top="0.423cm" fo:margin-bottom="0.212cm" fo:keep-with-next="always"/><style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="Heading_20_1" style:display-name="Heading 1" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="1" style:class="text"><style:text-properties fo:font-size="180%"/></style:style>
<style:style style:name="Heading_20_2" style:display-name="Heading 2" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="2" style:class="text"><style:text-properties fo:font-size="150%"/></style:style>
<style:style style:name="Heading_20_3" style:display-name="Heading 3" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="3" style:class="text"><style:text-properties fo:font-size="130%"/></style:style>
<style:style style:name="Heading_20_4" style:display-name="Heading 4" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="4" style:class="text"><style:text-properties fo:font-size="115%"/></style:style>
<style:style style:name="Heading_20_5" style:display-name="Heading 5" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="5" style:class="text"><style:text-properties fo:font-size="100%"/></style:style>
<style:style style:name="Heading_20_6" style:display-name="Heading 6" style:family="paragraph" style:parent-style-name="Heading" style:next-style-name="Text_20_body" style:default-outline-level="6" style:class="text"><style:text-properties fo:font-size="90%"/></style:style>
<style:style style:name="Quotations" style:family="paragraph" style:parent-style-name="Standard" style:class="html"><style:paragraph-properties fo:margin-left="1cm" fo:margin-right="1cm" fo:margin-top="0cm" fo:margin-bottom="0.247cm"/></style:style>
<style:style style:name="Preformatted_20_Text" style:display-name="Preformatted Text" style:family="paragraph" style:parent-style-name="Standard" style:class="html"><style:paragraph-properties fo:margin-top="0cm" fo:margin-bottom="0cm"/><style:text-properties style:font-name="Liberation Mono" fo:font-size="10pt"/></style:style>
<style:style style:name="Table_20_Contents" style:display-name="Table Contents" style:family="paragraph" style:parent-style-name="Standard" style:class="extra"/>
<style:style style:name="Table_20_Heading" style:display-name="Table Heading" style:family="paragraph" style:parent-style-name="Table_20_Contents" style:class="extra"><style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="Horizontal_20_Line" style:display-name="Horizontal Line" style:family="paragraph" style:parent-style-name="Standard" style:class="html"><style:paragraph-properties fo:margin-bottom="0.5cm" fo:border-bottom="0.5pt solid #808080" fo:padding="0cm"/><style:text-properties fo:font-size="6pt"/></style:style>
<style:style style:name="Emphasis" style:family="text"><style:text-properties fo:font-style="italic"/></style:style>
<style:style style:name="Strong_20_Emphasis" style:display-name="Strong Emphasis" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="Source_20_Text" style:display-name="Source Text" style:family="text"><style:text-properties style:font-name="Liberation Mono"/></style:style>
<style:style style:name="Internet_20_link" style:display-name="Internet link" style:family="text"><style:text-properties fo:color="#000080" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color"/></style:style>
<style:style style:name="Visited_20_Internet_20_Link" style:display-name="Visited Internet Link" style:family="text"><style:text-properties fo:color="#800000" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color"/></style:style>
<style:style style:name="Graphics" style:family="graphic"><style:graphic-properties text:anchor-type="as-char" svg:y="0cm" style:vertical-pos="top" style:vertical-rel="baseline"/></style:style>
</office:styles>
<office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="21cm" fo:page-height="29.7cm" style:print-orientation="portrait" fo:margin-top="2cm" fo:margin-bottom="2cm" fo:margin-left="2cm" fo:margin-right="2cm"/></style:page-layout></office:automatic-styles>
<office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1"/></office:master-styles>"##;

/// Automatic styles: strikethrough (no common style exists for it), aligned
/// table paragraphs, the table/cell/frame looks.
const AUTOMATIC_STYLES: &str = r##"<style:style style:name="Strike" style:family="text"><style:text-properties style:text-line-through-style="solid" style:text-line-through-type="single"/></style:style>
<style:style style:name="TD" style:family="paragraph" style:parent-style-name="Table_20_Contents"/>
<style:style style:name="TDc" style:family="paragraph" style:parent-style-name="Table_20_Contents"><style:paragraph-properties fo:text-align="center"/></style:style>
<style:style style:name="TDr" style:family="paragraph" style:parent-style-name="Table_20_Contents"><style:paragraph-properties fo:text-align="end"/></style:style>
<style:style style:name="TH" style:family="paragraph" style:parent-style-name="Table_20_Heading"/>
<style:style style:name="THc" style:family="paragraph" style:parent-style-name="Table_20_Heading"><style:paragraph-properties fo:text-align="center"/></style:style>
<style:style style:name="THr" style:family="paragraph" style:parent-style-name="Table_20_Heading"><style:paragraph-properties fo:text-align="end"/></style:style>
<style:style style:name="Tbl" style:family="table"><style:table-properties style:width="17cm" table:align="margins" fo:margin-top="0.2cm" fo:margin-bottom="0.3cm"/></style:style>
<style:style style:name="Cell" style:family="table-cell"><style:table-cell-properties fo:padding="0.1cm" fo:border="0.5pt solid #808080"/></style:style>
<style:style style:name="fr1" style:family="graphic" style:parent-style-name="Graphics"><style:graphic-properties style:vertical-pos="top" style:vertical-rel="baseline"/></style:style>"##;

/// `L1` bullets, `L2` numbers, ten levels each.
fn list_styles() -> String {
    let mut out = String::new();
    for (name, numbered) in [("L1", false), ("L2", true)] {
        out.push_str(&format!("<text:list-style style:name=\"{name}\">"));
        for level in 1..=10 {
            let indent = 0.635 * level as f64;
            let props = format!(
                "<style:list-level-properties text:list-level-position-and-space-mode=\"label-alignment\">\
                 <style:list-level-label-alignment text:label-followed-by=\"listtab\" \
                 text:list-tab-stop-position=\"{indent:.3}cm\" fo:text-indent=\"-0.635cm\" \
                 fo:margin-left=\"{indent:.3}cm\"/></style:list-level-properties>"
            );
            if numbered {
                out.push_str(&format!(
                    "<text:list-level-style-number text:level=\"{level}\" style:num-suffix=\".\" \
                     style:num-format=\"1\">{props}</text:list-level-style-number>"
                ));
            } else {
                let bullet = ["•", "◦", "▪"][(level - 1) % 3];
                out.push_str(&format!(
                    "<text:list-level-style-bullet text:level=\"{level}\" \
                     text:bullet-char=\"{bullet}\">{props}</text:list-level-style-bullet>"
                ));
            }
        }
        out.push_str("</text:list-style>");
    }
    out
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_xml_text(&mut out, s, &mut false, false);
    out
}

/// Append `s` as ODF character data. With `layout`, spaces that ODF would
/// collapse become `text:s`, and tabs/newlines their elements; `prev_space`
/// carries whether the output so far ends in (collapsible) whitespace.
fn push_xml_text(out: &mut String, s: &str, prev_space: &mut bool, layout: bool) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' if layout => {
                let mut n = 1;
                while chars.peek() == Some(&' ') {
                    chars.next();
                    n += 1;
                }
                if *prev_space {
                    out.push_str(&format!("<text:s text:c=\"{n}\"/>"));
                } else {
                    out.push(' ');
                    if n > 1 {
                        out.push_str(&format!("<text:s text:c=\"{}\"/>", n - 1));
                    }
                }
                *prev_space = true;
                continue;
            }
            '\t' if layout => out.push_str("<text:tab/>"),
            '\n' if layout => out.push_str("<text:line-break/>"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // XML 1.0 has no representation for other control characters.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n') => {}
            c => out.push(c),
        }
        *prev_space = layout && matches!(c, '\t' | '\n');
    }
}

/// A markdown destination as an ODF href: relative paths resolve from inside
/// the package, so a file beside the document is "../name".
fn odf_href(dest: &str) -> String {
    if !export::is_local(dest) {
        dest.to_string()
    } else if dest.starts_with('/') {
        format!("file://{}", encode_path(dest))
    } else {
        format!("../{}", encode_path(dest))
    }
}

/// Percent-encode what a URI can't hold literally: a markdown destination
/// written `<my file.pdf>` arrives with its space, which is no valid href.
/// `%` is left alone (the destination may already be encoded) and so is
/// `#`, which in a link is the fragment.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            ' ' | '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}' => {
                out.push_str(&format!("%{:02X}", c as u32))
            }
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn picture_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

struct TableOut {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    head: bool,
    cell: Option<String>,
    /// ODF has no tables inside list items: those come out as one
    /// paragraph per row.
    flat: bool,
}

struct OdtWriter<'a> {
    resolve: &'a dyn Fn(&str) -> Option<Vec<u8>>,
    body: String,
    /// (package path, media type, bytes)
    pictures: Vec<(String, &'static str, Vec<u8>)>,
    /// Closing tag of the open paragraph or heading.
    para: Option<&'static str>,
    prev_space: bool,
    quote: usize,
    lists: usize,
    list_start: Option<u64>,
    code: Option<String>,
    image: Option<(String, String)>,
    table: Option<TableOut>,
    tables: usize,
    /// Open `text:a` elements; an image that can't be embedded must not
    /// become a link inside one (a badge: `[![build](….svg)](ci)`).
    links: usize,
}

impl OdtWriter<'_> {
    fn in_cell(&self) -> bool {
        self.table.as_ref().is_some_and(|t| t.cell.is_some())
    }

    fn out(&mut self) -> &mut String {
        if self.in_cell() {
            self.table.as_mut().and_then(|t| t.cell.as_mut()).expect("checked")
        } else {
            &mut self.body
        }
    }

    fn raw(&mut self, xml: &str) {
        self.out().push_str(xml);
    }

    fn text(&mut self, s: &str) {
        let mut prev = self.prev_space;
        let mut xml = String::with_capacity(s.len());
        push_xml_text(&mut xml, s, &mut prev, true);
        self.prev_space = prev;
        self.raw(&xml);
    }

    fn body_style(&self) -> &'static str {
        if self.quote > 0 { "Quotations" } else { "Text_20_body" }
    }

    fn open_para(&mut self, style: &str) {
        self.close_para();
        self.raw(&format!("<text:p text:style-name=\"{style}\">"));
        self.para = Some("</text:p>");
        self.prev_space = true;
    }

    fn close_para(&mut self) {
        if let Some(closer) = self.para.take() {
            self.raw(closer);
        }
    }

    /// Inline content outside a paragraph (tight list items) gets one.
    fn ensure_para(&mut self) {
        if self.para.is_none() && !self.in_cell() {
            self.open_para(self.body_style());
        }
    }

    fn inline_text(&mut self, s: &str) {
        self.ensure_para();
        self.text(s);
    }

    fn run(&mut self, markdown: &str) {
        for event in Parser::new_ext(markdown, export::parser_options()) {
            self.event(event);
        }
        self.close_para();
    }

    fn event(&mut self, event: Event) {
        // Code block text and image alt text are collected, not written.
        if let Some(code) = &mut self.code {
            match &event {
                Event::Text(t) => return code.push_str(t),
                Event::End(TagEnd::CodeBlock) => {}
                _ => return,
            }
        }
        if let Some((_, alt)) = &mut self.image {
            match &event {
                Event::Text(t) | Event::Code(t) => return alt.push_str(t),
                Event::End(TagEnd::Image) => {}
                _ => return,
            }
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) | Event::InlineHtml(t) => self.inline_text(&t),
            Event::Code(t) => {
                self.ensure_para();
                self.raw("<text:span text:style-name=\"Source_20_Text\">");
                self.text(&t);
                self.raw("</text:span>");
            }
            Event::Html(t) => {
                self.open_para("Preformatted_20_Text");
                self.text(t.trim_end_matches('\n'));
                self.close_para();
            }
            Event::SoftBreak => self.inline_text(" "),
            Event::HardBreak => {
                self.ensure_para();
                self.raw("<text:line-break/>");
                self.prev_space = true;
            }
            Event::Rule => {
                self.close_para();
                self.raw("<text:p text:style-name=\"Horizontal_20_Line\"/>");
            }
            Event::TaskListMarker(done) => self.inline_text(if done { "☑ " } else { "☐ " }),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.open_para(self.body_style()),
            Tag::Heading { level, .. } => {
                self.close_para();
                let n = level as usize;
                self.raw(&format!(
                    "<text:h text:style-name=\"Heading_20_{n}\" text:outline-level=\"{n}\">"
                ));
                self.para = Some("</text:h>");
                self.prev_space = true;
            }
            Tag::BlockQuote(_) => {
                self.close_para();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.close_para();
                self.code = Some(String::new());
            }
            Tag::HtmlBlock => self.close_para(),
            Tag::List(start) => {
                self.close_para();
                self.lists += 1;
                let style = if start.is_some() { "L2" } else { "L1" };
                self.raw(&format!("<text:list text:style-name=\"{style}\">"));
                self.list_start = start.filter(|&s| s != 1);
            }
            Tag::Item => {
                self.close_para();
                match self.list_start.take() {
                    Some(s) => self.raw(&format!("<text:list-item text:start-value=\"{s}\">")),
                    None => self.raw("<text:list-item>"),
                }
            }
            Tag::Table(aligns) => {
                self.close_para();
                let flat = self.lists > 0;
                self.table = Some(TableOut { aligns, rows: Vec::new(), head: false, cell: None, flat });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cell = Some(String::new());
                }
                self.prev_space = true;
            }
            Tag::Emphasis => self.span("Emphasis"),
            Tag::Strong => self.span("Strong_20_Emphasis"),
            Tag::Strikethrough => self.span("Strike"),
            Tag::Link { dest_url, .. } => {
                self.links += 1;
                self.ensure_para();
                self.raw(&format!(
                    "<text:a xlink:type=\"simple\" xlink:href=\"{}\" \
                     text:style-name=\"Internet_20_link\" \
                     text:visited-style-name=\"Visited_20_Internet_20_Link\">",
                    esc(&odf_href(&dest_url))
                ));
            }
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn span(&mut self, style: &str) {
        self.ensure_para();
        self.raw(&format!("<text:span text:style-name=\"{style}\">"));
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.close_para(),
            TagEnd::BlockQuote(_) => {
                self.close_para();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                let code = self.code.take().unwrap_or_default();
                for line in code.strip_suffix('\n').unwrap_or(&code).split('\n') {
                    self.open_para("Preformatted_20_Text");
                    self.text(line);
                    self.close_para();
                }
            }
            TagEnd::List(_) => {
                self.close_para();
                self.raw("</text:list>");
                self.lists = self.lists.saturating_sub(1);
            }
            TagEnd::Item => {
                self.close_para();
                self.raw("</text:list-item>");
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    if let (Some(cell), Some(row)) = (t.cell.take(), t.rows.last_mut()) {
                        row.push(cell);
                    }
                }
            }
            TagEnd::Table => self.close_table(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.raw("</text:span>"),
            TagEnd::Link => {
                self.links = self.links.saturating_sub(1);
                self.raw("</text:a>");
            }
            TagEnd::Image => self.close_image(),
            _ => {}
        }
    }

    fn close_table(&mut self) {
        let Some(t) = self.table.take() else { return };
        if t.flat {
            for row in &t.rows {
                self.raw(&format!("<text:p text:style-name=\"Text_20_body\">{}</text:p>", row.join(" | ")));
            }
            return;
        }
        let cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        self.tables += 1;
        let mut xml = format!(
            "<table:table table:name=\"Table{}\" table:style-name=\"Tbl\">\
             <table:table-column table:number-columns-repeated=\"{cols}\"/>",
            self.tables
        );
        for (i, row) in t.rows.iter().enumerate() {
            let head = i == 0 && t.head;
            if head {
                xml.push_str("<table:table-header-rows>");
            }
            xml.push_str("<table:table-row>");
            for c in 0..cols {
                let style = match (head, t.aligns.get(c)) {
                    (true, Some(Alignment::Center)) => "THc",
                    (true, Some(Alignment::Right)) => "THr",
                    (true, _) => "TH",
                    (false, Some(Alignment::Center)) => "TDc",
                    (false, Some(Alignment::Right)) => "TDr",
                    (false, _) => "TD",
                };
                xml.push_str(&format!(
                    "<table:table-cell table:style-name=\"Cell\" office:value-type=\"string\">\
                     <text:p text:style-name=\"{style}\">{}</text:p></table:table-cell>",
                    row.get(c).map(String::as_str).unwrap_or("")
                ));
            }
            xml.push_str("</table:table-row>");
            if head {
                xml.push_str("</table:table-header-rows>");
            }
        }
        xml.push_str("</table:table>");
        self.raw(&xml);
    }

    fn close_image(&mut self) {
        let Some((dest, alt)) = self.image.take() else { return };
        self.ensure_para();
        let embedded = export::is_local(&dest)
            .then(|| (self.resolve)(&dest))
            .flatten()
            .and_then(|bytes| {
                let pixbuf = export::pixbuf_from_bytes(&bytes)?;
                let (w, h) = (pixbuf.width(), pixbuf.height());
                (w > 0 && h > 0).then_some((bytes, w, h))
            });
        let Some((bytes, w, h)) = embedded else {
            // Remote or unreadable: keep it reachable as a link — unless it
            // already sits in one, where its alt text is the link's label.
            let label = if alt.trim().is_empty() { dest.clone() } else { alt };
            if self.links > 0 {
                return self.text(&label);
            }
            self.raw(&format!("<text:a xlink:type=\"simple\" xlink:href=\"{}\">", esc(&odf_href(&dest))));
            self.text(&label);
            return self.raw("</text:a>");
        };
        let mime = export::image_mime(&bytes);
        let n = self.pictures.len() + 1;
        let name = format!("Pictures/image{n}.{}", picture_ext(mime));
        // Pixels at 96 dpi, shrunk to fit the text area.
        let (w_cm, h_cm) = (w as f64 / 96.0 * 2.54, h as f64 / 96.0 * 2.54);
        let scale = (TEXT_WIDTH_CM / w_cm).min(TEXT_HEIGHT_CM / h_cm).min(1.0);
        let title = if alt.is_empty() { String::new() } else { format!("<svg:title>{}</svg:title>", esc(&alt)) };
        self.raw(&format!(
            "<draw:frame draw:style-name=\"fr1\" draw:name=\"Image{n}\" text:anchor-type=\"as-char\" \
             svg:width=\"{:.3}cm\" svg:height=\"{:.3}cm\" draw:z-index=\"0\">\
             <draw:image xlink:href=\"{name}\" xlink:type=\"simple\" xlink:show=\"embed\" \
             xlink:actuate=\"onLoad\"/>{title}</draw:frame>",
            w_cm * scale,
            h_cm * scale
        ));
        self.prev_space = false;
        self.pictures.push((name, mime, bytes));
    }
}

/// Write `text` as an .odt at `path`: markdown with its structure, plain
/// kinds as preformatted lines. Atomic like pack saves — a temp file renamed
/// over the destination.
pub fn write_odt(path: &Path, title: &str, text: &str, content: Content) -> Result<(), String> {
    let (body, pictures) = match content {
        Content::Markdown { resolve } => {
            let mut w = OdtWriter {
                resolve,
                body: String::new(),
                pictures: Vec::new(),
                para: None,
                prev_space: true,
                quote: 0,
                lists: 0,
                list_start: None,
                code: None,
                image: None,
                table: None,
                tables: 0,
                links: 0,
            };
            w.run(text);
            (w.body, w.pictures)
        }
        Content::Plain => {
            let mut body = String::new();
            for line in text.split('\n') {
                body.push_str("<text:p text:style-name=\"Preformatted_20_Text\">");
                push_xml_text(&mut body, line, &mut true, true);
                body.push_str("</text:p>");
            }
            (body, Vec::new())
        }
    };
    match crate::doc::replace_via_temp(path, |file| write_package(file, title, &body, &pictures)) {
        Ok(()) => Ok(()),
        Err(crate::doc::Replace::Unsupported(err) | crate::doc::Replace::Failed(err)) => {
            Err(err.to_string())
        }
    }
}

fn write_package(
    dest: &mut File,
    title: &str,
    body: &str,
    pictures: &[(String, &'static str, Vec<u8>)],
) -> io::Result<()> {
    let content = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-content {NAMESPACES} \
         office:version=\"1.3\"><office:automatic-styles>{AUTOMATIC_STYLES}{}</office:automatic-styles>\
         <office:body><office:text>{body}</office:text></office:body></office:document-content>",
        list_styles()
    );
    let styles = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-styles {NAMESPACES} \
         office:version=\"1.3\">{STYLES_BODY}</office:document-styles>"
    );
    let meta = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<office:document-meta {NAMESPACES} \
         office:version=\"1.3\"><office:meta><meta:generator>Bindle</meta:generator>\
         <dc:title>{}</dc:title></office:meta></office:document-meta>",
        esc(title)
    );
    let mut manifest = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<manifest:manifest \
         xmlns:manifest=\"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0\" manifest:version=\"1.3\">\
         <manifest:file-entry manifest:full-path=\"/\" manifest:version=\"1.3\" \
         manifest:media-type=\"application/vnd.oasis.opendocument.text\"/>\
         <manifest:file-entry manifest:full-path=\"content.xml\" manifest:media-type=\"text/xml\"/>\
         <manifest:file-entry manifest:full-path=\"styles.xml\" manifest:media-type=\"text/xml\"/>\
         <manifest:file-entry manifest:full-path=\"meta.xml\" manifest:media-type=\"text/xml\"/>",
    );
    for (name, mime, _) in pictures {
        manifest.push_str(&format!(
            "<manifest:file-entry manifest:full-path=\"{name}\" manifest:media-type=\"{mime}\"/>"
        ));
    }
    manifest.push_str("</manifest:manifest>");

    let mut zip = ZipWriter::new(dest);
    // The package spec: "mimetype" first and uncompressed, so the type can
    // be sniffed at a fixed offset.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default();
    zip.start_file("mimetype", stored).map_err(io::Error::other)?;
    zip.write_all(b"application/vnd.oasis.opendocument.text")?;
    for (name, xml) in [
        ("content.xml", &content),
        ("styles.xml", &styles),
        ("meta.xml", &meta),
        ("META-INF/manifest.xml", &manifest),
    ] {
        zip.start_file(name, deflated).map_err(io::Error::other)?;
        zip.write_all(xml.as_bytes())?;
    }
    // Pictures are compressed formats already.
    for (name, _, bytes) in pictures {
        zip.start_file(name.as_str(), stored).map_err(io::Error::other)?;
        zip.write_all(bytes)?;
    }
    zip.finish().map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_odt(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("bindle-odf-{name}-{}.odt", std::process::id()))
    }

    #[test]
    fn column_alignment_needs_a_clear_majority() {
        use Align::*;
        let col = |cells: &[Option<Align>]| {
            column_aligns(&cells.iter().map(|c| vec![*c]).collect::<Vec<_>>())[0]
        };
        assert_eq!(col(&[Some(Right), Some(Right), Some(Center)]), Right);
        // Center vs Right tie used to come out Center.
        assert_eq!(col(&[Some(Center), Some(Right)]), None);
        assert_eq!(col(&[Some(Center), Some(None)]), None);
        // Empty cells (no alignment recorded) don't vote.
        assert_eq!(col(&[Some(Center), Option::None, Option::None]), Center);
    }

    /// Markdown already in the importer's canonical form survives a trip
    /// through an exported .odt unchanged — the importer reading the
    /// exporter's styles is what this pins down.
    fn round_trip(md: &str, name: &str) -> String {
        let path = temp_odt(name);
        let resolve = |_: &str| None;
        write_odt(&path, "T", md, Content::Markdown { resolve: &resolve }).expect("written");
        let back = import(&path).expect("imported");
        let _ = std::fs::remove_file(&path);
        back
    }

    #[test]
    fn a_web_image_inside_a_link_does_not_nest_links() {
        let back = round_trip("[![build](https://x.test/badge.svg)](https://x.test/ci)\n", "badge");
        assert_eq!(back, "[build](https://x.test/ci)\n");
    }

    #[test]
    fn a_trailing_hash_in_a_heading_survives() {
        assert_eq!(round_trip("## Issue \\#\n\n## C#\n", "hash"), "## Issue \\#\n\n## C#\n");
        assert_eq!(escape_closing_hashes("#"), "\\#");
        assert_eq!(escape_closing_hashes("a ##"), "a \\##");
    }

    #[test]
    fn local_hrefs_are_percent_encoded() {
        assert_eq!(odf_href("my file.pdf"), "../my%20file.pdf");
        assert_eq!(odf_href("/tmp/a b.pdf"), "file:///tmp/a%20b.pdf");
        assert_eq!(odf_href("notes.md#part"), "../notes.md#part");
        assert_eq!(odf_href("https://x.test/a b"), "https://x.test/a b");
    }

    #[test]
    fn export_then_import_round_trips() {
        let md = "\
# Title

Some **bold**, *italic*, ~~strike~~ and `code` with a [link](https://example.com) and [notes](notes.pdf).

## Lists

- one
- two
  - nested

1. first
2. second

> quoted text

```
fn main() {
    println!(\"hi\");
}
```

| A | B | C |
| --- | :---: | ---: |
| 1 | 2 \\| 3 | 4 |

Line one\\
line  two & \\<more>
";
        let path = temp_odt("roundtrip");
        let resolve = |_: &str| None;
        write_odt(&path, "T", md, Content::Markdown { resolve: &resolve }).expect("written");
        let back = import(&path).expect("imported");
        let _ = std::fs::remove_file(&path);
        assert_eq!(back, md);
    }

    #[test]
    fn package_starts_with_stored_mimetype_and_embeds_pictures() {
        let pixbuf = gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, 96, 48)
            .expect("pixbuf");
        let png = pixbuf.save_to_bufferv("png", &[]).expect("png");
        let resolve = move |dest: &str| (dest == "pic.png").then(|| png.clone());
        let path = temp_odt("picture");
        write_odt(&path, "T", "![A cat](pic.png)\n", Content::Markdown { resolve: &resolve })
            .expect("written");
        let mut zip = ZipArchive::new(File::open(&path).expect("open")).expect("zip");
        let first = zip.by_index(0).expect("entry");
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), CompressionMethod::Stored);
        drop(first);
        assert!(zip.by_name("Pictures/image1.png").is_ok());
        let back = import(&path).expect("imported");
        let _ = std::fs::remove_file(&path);
        assert_eq!(back, "![A cat](Pictures/image1.png)\n");
    }

    /// The shape LibreOffice writes (taken from a real conversion): headings
    /// as styled paragraphs, emphasis via automatic styles, list styles in
    /// content.xml, code as Preformatted Text paragraphs with text:s.
    const LIBREOFFICE: &str = r#"<office:document-content><office:automatic-styles>
<style:style style:name="P1" style:family="paragraph" style:parent-style-name="Heading_20_1"/>
<style:style style:name="P2" style:family="paragraph" style:parent-style-name="Text_20_body" style:list-style-name="L1"><style:paragraph-properties fo:margin-top="0cm"/></style:style>
<style:style style:name="P6" style:family="paragraph" style:parent-style-name="Preformatted_20_Text"/>
<style:style style:name="P7" style:family="paragraph" style:parent-style-name="Table_20_Contents"><style:paragraph-properties fo:text-align="end"/><style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="P8" style:family="paragraph" style:parent-style-name="Table_20_Contents"><style:paragraph-properties fo:text-align="end"/></style:style>
<style:style style:name="T1" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="T2" style:family="text"><style:text-properties fo:font-style="italic"/></style:style>
<style:style style:name="T3" style:family="text"><style:text-properties style:text-line-through-style="solid"/></style:style>
<text:list-style style:name="L1"><text:list-level-style-bullet text:level="1" text:bullet-char="•"/><text:list-level-style-bullet text:level="2" text:bullet-char="•"/></text:list-style>
<text:list-style style:name="L2"><text:list-level-style-number text:level="1" style:num-format="1"/></text:list-style>
</office:automatic-styles><office:body><office:text>
<text:sequence-decls><text:sequence-decl text:name="Table"/></text:sequence-decls>
<text:p text:style-name="P1">Report title</text:p>
<text:p text:style-name="Text_20_body">Plain text with <text:span text:style-name="T1">bold</text:span>, <text:span text:style-name="T2">italic</text:span>, <text:span text:style-name="T3">struck</text:span>, <text:span text:style-name="Source_20_Text">code_span</text:span> and a <text:a xlink:type="simple" xlink:href="https://example.com/a%20b">link</text:a>. Snake_case_word and 1 * 2 stay literal.</text:p>
<text:h text:style-name="Heading_20_2" text:outline-level="2">Lists</text:h>
<text:list text:style-name="L1">
<text:list-item><text:p text:style-name="P2">first bullet </text:p></text:list-item>
<text:list-item><text:p text:style-name="P2">second bullet </text:p>
<text:list><text:list-item><text:p text:style-name="P2">nested bullet</text:p></text:list-item></text:list>
</text:list-item>
</text:list>
<text:list text:style-name="L2">
<text:list-item><text:p>first number </text:p></text:list-item>
<text:list-item><text:p>second number </text:p></text:list-item>
</text:list>
<text:p text:style-name="Quotations">A quoted paragraph.</text:p>
<text:p text:style-name="Preformatted_20_Text">fn main() {</text:p>
<text:p text:style-name="Preformatted_20_Text"><text:s text:c="4"/>println!("hi");</text:p>
<text:p text:style-name="P6">}</text:p>
<text:h text:style-name="Heading_20_3" text:outline-level="3">Table</text:h>
<table:table table:name="Table1">
<table:table-column/><table:table-column/>
<table:table-row>
<table:table-cell><text:p text:style-name="Table_20_Heading">Name</text:p></table:table-cell>
<table:table-cell><text:p text:style-name="Table_20_Heading">Value</text:p></table:table-cell>
</table:table-row>
<table:table-row>
<table:table-cell><text:p text:style-name="Table_20_Contents">alpha</text:p></table:table-cell>
<table:table-cell><text:p text:style-name="P8">1 | 2</text:p></table:table-cell>
</table:table-row>
<table:table-row>
<table:table-cell><text:p text:style-name="Table_20_Contents">beta</text:p></table:table-cell>
<table:table-cell><text:p text:style-name="P7">two</text:p></table:table-cell>
<table:covered-table-cell table:number-columns-repeated="3"/>
</table:table-row>
</table:table>
<text:p text:style-name="Text_20_body">Line one<text:line-break/>line two</text:p>
</office:text></office:body></office:document-content>"#;

    #[test]
    fn libreoffice_document_imports() {
        let md = to_markdown(LIBREOFFICE, "").expect("imports");
        let expected = "\
# Report title

Plain text with **bold**, *italic*, ~~struck~~, `code_span` and a [link](https://example.com/a%20b). Snake_case_word and 1 \\* 2 stay literal.

## Lists

- first bullet
- second bullet
  - nested bullet

1. first number
2. second number

> A quoted paragraph.

```
fn main() {
    println!(\"hi\");
}
```

### Table

| Name | Value |
| --- | ---: |
| alpha | 1 \\| 2 |
| beta | **two** |

Line one\\
line two
";
        assert_eq!(md, expected);
    }

    #[test]
    fn noise_is_dropped_and_notes_images_links_survive() {
        let content = r##"<office:text>
<text:tracked-changes><text:changed-region text:id="c1"><text:deletion><text:p>deleted words</text:p></text:deletion></text:changed-region></text:tracked-changes>
<text:p>a<text:s text:c="2"/>b   c<office:annotation><text:p>a comment</text:p></office:annotation> d<text:note text:note-class="footnote"><text:note-citation>1</text:note-citation><text:note-body><text:p>The note.</text:p></text:note-body></text:note>.</text:p>
<text:p># not a heading &amp; <text:span text:style-name="Emphasis"> spaced </text:span>end</text:p>
<text:p>- not a list, 1 * 2, snake_case, [x] &#169;</text:p>
<text:p><draw:frame draw:name="Image1"><draw:image xlink:href="Pictures/pic%201.png"/><svg:title>A picture</svg:title></draw:frame></text:p>
<text:p text:style-name="Standard"/>
<text:h text:outline-level="2"><text:number>2.</text:number>Numbered heading</text:h>
<text:p>see <text:a xlink:href="../notes.pdf">the notes</text:a></text:p>
<text:table-of-content><text:index-body><text:p>Numbered heading	2</text:p></text:index-body></text:table-of-content>
</office:text>"##;
        let md = to_markdown(content, "").expect("imports");
        let expected = "\
a  b c d[^1].

\\# not a heading & *spaced* end

\\- not a list, 1 \\* 2, snake_case, \\[x\\] ©

![A picture](Pictures/pic%201.png)

## Numbered heading

see [the notes](notes.pdf)

[^1]: The note.
";
        assert_eq!(md, expected);
    }

    #[test]
    fn malformed_xml_is_an_error() {
        assert!(to_markdown("<office:text><text:p>open</office:text>", "").is_err());
    }
}
