//! Keyword highlighting for the editor: approximate, line-based scanners
//! that turn document text into char-offset spans; `ui.rs` applies them as
//! text tags on the editor buffer. Deliberately not a full parser — the goal
//! is orientation while editing, not lossless syntax analysis.

use gtk4::prelude::*;

use crate::doc::DocKind;

/// A highlight run in buffer char offsets, tagged with one of `TAGS`.
pub struct Span {
    pub start: i32,
    pub end: i32,
    pub tag: &'static str,
}

/// Every tag `spans` can emit; `apply` removes exactly these before
/// re-applying, so stale highlighting never lingers.
pub const TAGS: &[&str] = &[
    "hl-heading",
    "hl-marker",
    "hl-code",
    "hl-strong",
    "hl-emph",
    "hl-link",
    "hl-url",
    "hl-key",
    "hl-string",
    "hl-number",
    "hl-bool",
    "hl-comment",
    "hl-section",
];

/// Create the editor's highlight tags. Call once per buffer.
///
/// Only the weight/style properties are set here; the colours come from
/// [`retint`], which `ui.rs` calls straight after and again on every theme or
/// light/dark change.
pub fn setup_tags(buffer: &gtk4::TextBuffer) {
    use gtk4::TextTag;
    let table = buffer.tag_table();
    let add = |tag: TextTag| {
        table.add(&tag);
    };
    add(TextTag::builder().name("hl-heading").weight(800).build());
    add(TextTag::builder().name("hl-marker").weight(700).build());
    add(TextTag::builder().name("hl-code").build());
    add(TextTag::builder().name("hl-strong").weight(700).build());
    add(TextTag::builder()
        .name("hl-emph")
        .style(gtk4::pango::Style::Italic)
        .build());
    add(TextTag::builder()
        .name("hl-link")
        .underline(gtk4::pango::Underline::Single)
        .build());
    add(TextTag::builder().name("hl-url").build());
    add(TextTag::builder().name("hl-key").build());
    add(TextTag::builder().name("hl-string").build());
    add(TextTag::builder().name("hl-number").build());
    add(TextTag::builder().name("hl-bool").weight(600).build());
    add(TextTag::builder()
        .name("hl-comment")
        .style(gtk4::pango::Style::Italic)
        .build());
    add(TextTag::builder().name("hl-section").weight(700).build());
}

/// Point the coloured `hl-*` tags at `colors`. Safe to call at any time:
/// setting a tag property restyles the text already tagged with it, and
/// (unlike editing the buffer) emits no `changed`, so the document is not
/// marked modified and undo is untouched.
pub fn retint(buffer: &gtk4::TextBuffer, colors: &crate::theme::Tags) {
    let table = buffer.tag_table();
    let set = |name: &str, color: &str| {
        if let Some(tag) = table.lookup(name) {
            tag.set_foreground(Some(color));
        }
    };
    set("hl-heading", colors.heading);
    set("hl-marker", colors.marker);
    set("hl-code", colors.code);
    set("hl-link", colors.link);
    set("hl-url", colors.url);
    set("hl-key", colors.key);
    set("hl-string", colors.string);
    set("hl-number", colors.number);
    set("hl-bool", colors.boolean);
    set("hl-comment", colors.comment);
    set("hl-section", colors.section);
}

/// Highlight spans for `text` interpreted as `kind`. Char offsets.
///
/// No returned span crosses a newline — [`split_at_newlines`] enforces that
/// after the scanner has run, whatever the scanner did. [`line_sigs`] relies
/// on it to attribute every span to exactly one line, which is what makes the
/// incremental re-tagging in `ui.rs` sound.
pub fn spans(text: &str, kind: DocKind) -> Vec<Span> {
    let raw = match kind {
        DocKind::Markdown => markdown(text),
        DocKind::Json => json(text),
        DocKind::Yaml => yaml(text),
        DocKind::Toml => toml(text),
        DocKind::Shell => shell(text),
        DocKind::Xml => xml(text),
        DocKind::Text | DocKind::Nfo => Vec::new(),
    };
    split_at_newlines(text, raw)
}

/// The characters GTK's `TextBuffer` ends a line on. `\r\n` is one break
/// made of two of them.
fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2029}')
}

/// Char offset of every line-break character in `text`, ascending.
fn newline_offsets(text: &str) -> Vec<i32> {
    text.chars()
        .enumerate()
        .filter(|(_, c)| is_break(*c))
        .map(|(i, _)| i as i32)
        .collect()
}

/// Lines as GTK counts them — `\n`, `\r`, `\r\n` and U+2029 all end one —
/// as (char offset of the line start, text without its terminator). The
/// line indices `track_edits` gets from GTK must name the same lines as
/// these, or a lone `\r` from a paste would shift every index after it.
fn gtk_lines(text: &str) -> Vec<(i32, &str)> {
    let mut out = Vec::new();
    let (mut start_byte, mut start_char, mut pos) = (0usize, 0i32, 0i32);
    let mut iter = text.char_indices().peekable();
    while let Some((b, c)) = iter.next() {
        pos += 1;
        if !is_break(c) {
            continue;
        }
        let mut next_byte = b + c.len_utf8();
        if c == '\r' && matches!(iter.peek(), Some((_, '\n'))) {
            iter.next();
            next_byte += 1;
            pos += 1;
        }
        out.push((start_char, &text[start_byte..b]));
        (start_byte, start_char) = (next_byte, pos);
    }
    out.push((start_char, &text[start_byte..]));
    out
}

/// Line breaks in `text` as GTK counts them (`\r\n` once).
fn line_breaks(text: &str) -> usize {
    text.matches('\n').count() + text.matches('\u{2029}').count() + text.matches('\r').count()
        - text.matches("\r\n").count()
}

/// Cut any span that runs past a newline into one span per line it covers.
///
/// The line-based scanners never produce such a span, but the char-based ones
/// can on malformed input — an unterminated JSON string or XML comment runs to
/// the next delimiter, lines away.
fn split_at_newlines(text: &str, spans: Vec<Span>) -> Vec<Span> {
    if !spans.iter().any(|s| s.end - s.start > 1) {
        return spans;
    }
    let newlines = newline_offsets(text);
    if newlines.is_empty() {
        return spans;
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        let mut start = span.start;
        // First break at or after the span start: one *starting* on a break
        // char still crosses into the next line.
        let mut i = newlines.partition_point(|&n| n < start);
        while i < newlines.len() && newlines[i] < span.end {
            push(&mut out, start, newlines[i], span.tag);
            start = newlines[i] + 1;
            i += 1;
        }
        push(&mut out, start, span.end, span.tag);
    }
    out
}

/// Per-line (char offset of the line start, signature of its highlighting).
///
/// The signature folds in both the line's text and the spans on it, so it
/// changes whenever that line needs re-tagging — including when the text is
/// untouched but the highlighting moved under it (typing the opening ``` of a
/// fence restyles every line below without editing one of them). Spans are
/// combined with XOR on offsets *relative to the line start*, so a signature
/// says nothing about where in the document the line sits: shifting a line up
/// or down by an edit elsewhere leaves it identical, which is exactly right —
/// GTK's tags travel with the text, so such a line needs no work at all.
pub fn line_sigs(text: &str, spans: &[Span]) -> (Vec<i32>, Vec<u64>) {
    let mut starts = Vec::new();
    let mut sigs = Vec::new();
    for (start, line) in gtk_lines(text) {
        starts.push(start);
        sigs.push(fnv(line.as_bytes(), FNV_OFFSET));
    }
    for span in spans {
        // The line this span starts on; spans never cross a newline.
        let i = starts.partition_point(|&s| s <= span.start) - 1;
        let base = span.start - starts[i];
        let mut h = fnv(span.tag.as_bytes(), FNV_OFFSET);
        h = fnv(&base.to_le_bytes(), h);
        h = fnv(&(span.end - starts[i]).to_le_bytes(), h);
        // XOR: order-independent, so the scanners are free to emit a line's
        // spans in whatever order suits them (markdown's inline pass does not
        // emit them left to right) without disturbing the signature.
        sigs[i] ^= h;
    }
    // EDITED is reserved for "retag this line"; no real signature may equal it.
    for sig in &mut sigs {
        if *sig == EDITED {
            *sig = 1;
        }
    }
    (starts, sigs)
}

/// Baseline entry of a line edited since the last pass: its tags are
/// whatever GTK's insert/delete left there, so it matches no signature.
const EDITED: u64 = 0;

/// Keep a baseline lined up with the buffer's lines across an insertion of
/// text holding `newlines` line breaks on `line`. The baseline must follow
/// line *identity*, not position: comparing signatures by index alone let a
/// multi-line delete put an untouched line against a different old line with
/// the same signature, keeping tags it should have lost (text after a removed
/// fence stayed code-coloured). Each baseline entry now describes exactly
/// the tags on the buffer line at that index.
pub fn note_insert(baseline: &mut Vec<u64>, line: usize, newlines: usize) {
    if baseline.is_empty() {
        return;
    }
    if line >= baseline.len() {
        // Out of step somehow: an empty baseline forces a full pass.
        baseline.clear();
        return;
    }
    baseline[line] = EDITED;
    baseline.splice(line + 1..line + 1, std::iter::repeat(EDITED).take(newlines));
}

/// The deletion counterpart of [`note_insert`]: lines `first..=last` merge
/// into `first`, which is now edited.
pub fn note_delete(baseline: &mut Vec<u64>, first: usize, last: usize) {
    if baseline.is_empty() {
        return;
    }
    if first > last || last >= baseline.len() {
        baseline.clear();
        return;
    }
    baseline.drain(first + 1..=last);
    baseline[first] = EDITED;
}

/// Feed every insertion and deletion in `buffer` into `baseline`. Connected
/// before GTK's own handlers run, so the iterators still describe the text
/// as it was before the edit.
pub fn track_edits(buffer: &gtk4::TextBuffer, baseline: std::rc::Rc<std::cell::RefCell<Vec<u64>>>) {
    let inserts = baseline.clone();
    buffer.connect_insert_text(move |_, at, text| {
        note_insert(&mut inserts.borrow_mut(), at.line() as usize, line_breaks(text));
    });
    buffer.connect_delete_range(move |_, start, end| {
        note_delete(&mut baseline.borrow_mut(), start.line() as usize, end.line() as usize);
    });
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(bytes: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for b in bytes {
        h = (h ^ *b as u64).wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// The range of lines whose highlighting differs between two signature runs,
/// as `[first, last]` indices into `new` — `None` when nothing changed.
/// `ui.rs` passes a baseline kept aligned by [`track_edits`], so the two are
/// the same length and this is simply the first to last mismatching line.
///
/// Matching signatures are trimmed from both ends first, so inserting a line
/// leaves just that line dirty however long the document is. Anything in
/// between is treated as one contiguous range: the scattered case barely
/// happens (cross-line state changes affect a *suffix*), and covering it in
/// one remove/re-apply pass is cheaper than being clever about it.
pub fn dirty_lines(old: &[u64], new: &[u64]) -> Option<(usize, usize)> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    if head == new.len() {
        // Every remaining line is unchanged and only trailing lines went away
        // (Enter at the end of the document, then Backspace): the removed
        // lines took their tags with them. Falling through would return a
        // range starting one past the last line, which `apply_highlight`
        // indexes — that panic aborted the app.
        return None;
    }
    let tail = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(old.len() - head)
        .min(new.len() - head);
    if head + tail >= new.len() && old.len() == new.len() {
        return None;
    }
    Some((head, new.len().saturating_sub(tail).saturating_sub(1).max(head)))
}

/// Iterate lines as (char offset of line start, chars of the line).
fn lines(text: &str) -> impl Iterator<Item = (i32, Vec<char>)> + '_ {
    let mut offset = 0i32;
    text.split('\n').map(move |line| {
        let chars: Vec<char> = line.chars().collect();
        let start = offset;
        offset += chars.len() as i32 + 1;
        (start, chars)
    })
}

fn push(out: &mut Vec<Span>, start: i32, end: i32, tag: &'static str) {
    if end > start {
        out.push(Span { start, end, tag });
    }
}

// --- Markdown ---

fn markdown(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    // The open fence's character and length: only a run of the same
    // character, at least as long and with nothing after it, closes it.
    let mut fence: Option<(char, usize)> = None;
    for (base, chars) in lines(text) {
        let n = chars.len();
        let end = base + n as i32;
        let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
        let rest = &chars[indent..];
        let ch = rest.first().copied().filter(|c| matches!(c, '`' | '~'));
        let run = ch.map_or(0, |ch| rest.iter().take_while(|c| **c == ch).count());
        let fence_line = match (fence, ch) {
            (None, Some(ch)) if run >= 3 => {
                fence = Some((ch, run));
                true
            }
            (Some((open, len)), Some(ch))
                if ch == open && run >= len && rest[run..].iter().all(|c| c.is_whitespace()) =>
            {
                fence = None;
                true
            }
            _ => false,
        };
        if fence_line || fence.is_some() {
            push(&mut out, base, end, "hl-code");
            continue;
        }
        // Headings: whole line. Four columns of indent make it indented
        // code instead.
        let cols: usize = chars[..indent].iter().map(|c| if *c == '\t' { 4 } else { 1 }).sum();
        let hashes = rest.iter().take_while(|c| **c == '#').count();
        if cols < 4
            && (1..=6).contains(&hashes)
            && rest.get(hashes).map(|c| *c == ' ').unwrap_or(true)
        {
            push(&mut out, base, end, "hl-heading");
            continue;
        }
        let mut i = indent;
        // Blockquote markers, possibly nested ("> > text").
        while i < n && chars[i] == '>' {
            push(&mut out, base + i as i32, base + i as i32 + 1, "hl-marker");
            i += 1;
            while i < n && chars[i] == ' ' {
                i += 1;
            }
        }
        // List markers: bullets, numbers, task boxes.
        if i + 1 < n && matches!(chars[i], '-' | '*' | '+') && chars[i + 1] == ' ' {
            let mut m_end = i + 1;
            if chars.get(i + 2) == Some(&'[')
                && matches!(chars.get(i + 3), Some(' ' | 'x' | 'X'))
                && chars.get(i + 4) == Some(&']')
            {
                m_end = i + 5;
            }
            push(&mut out, base + i as i32, base + m_end as i32, "hl-marker");
            i = m_end;
        } else {
            let digits = chars[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0
                && matches!(chars.get(i + digits), Some('.' | ')'))
                && chars.get(i + digits + 1) == Some(&' ')
            {
                push(
                    &mut out,
                    base + i as i32,
                    base + (i + digits + 1) as i32,
                    "hl-marker",
                );
                i += digits + 1;
            }
        }
        inline_markdown(&chars, i, base, &mut out);
    }
    out
}

/// Inline markdown on one line: code spans, links, bold, italic.
fn inline_markdown(chars: &[char], from: usize, base: i32, out: &mut Vec<Span>) {
    let n = chars.len();
    let mut used = vec![false; n];
    let mark = |a: usize, b: usize, used: &mut Vec<bool>| {
        for slot in used.iter_mut().take(b).skip(a) {
            *slot = true;
        }
    };
    // `_` never opens or closes emphasis inside a word (`snake_case_name`);
    // `*` may. Both look at the char just outside the delimiter run.
    let word_at = |k: Option<usize>| k.and_then(|k| chars.get(k)).is_some_and(|c| c.is_alphanumeric());
    let opens = |d: char, i: usize| d == '*' || !word_at(i.checked_sub(1));
    let closes = |d: char, after: usize| d == '*' || !word_at(Some(after));
    // `code` spans first; nothing else matches inside them.
    let mut i = from;
    while i < n {
        if chars[i] == '`' {
            if let Some(j) = (i + 1..n).find(|&j| chars[j] == '`') {
                push(out, base + i as i32, base + j as i32 + 1, "hl-code");
                mark(i, j + 1, &mut used);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    // Links and images: [text](dest).
    let mut i = from;
    while i < n {
        if chars[i] == '[' && !used[i] {
            if let Some(j) = (i + 1..n).find(|&j| chars[j] == ']' && !used[j]) {
                if chars.get(j + 1) == Some(&'(') {
                    if let Some(k) = (j + 2..n).find(|&k| chars[k] == ')' && !used[k]) {
                        push(out, base + i as i32, base + j as i32 + 1, "hl-link");
                        push(out, base + j as i32 + 1, base + k as i32 + 1, "hl-url");
                        mark(i, k + 1, &mut used);
                        i = k + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    // **bold** / __bold__.
    for d in ['*', '_'] {
        let mut i = from;
        while i + 1 < n {
            if !used[i] && chars[i] == d && chars[i + 1] == d && opens(d, i) {
                if let Some(j) = (i + 2..n - 1).find(|&j| {
                    chars[j] == d && chars[j + 1] == d && !used[j] && closes(d, j + 2)
                }) {
                    push(out, base + i as i32, base + j as i32 + 2, "hl-strong");
                    mark(i, j + 2, &mut used);
                    i = j + 2;
                    continue;
                }
            }
            i += 1;
        }
    }
    // *italic* / _italic_ (delimiters hugging the content).
    for d in ['*', '_'] {
        let mut i = from;
        while i < n {
            if !used[i]
                && chars[i] == d
                && chars.get(i + 1).map(|c| !c.is_whitespace() && *c != d).unwrap_or(false)
                && opens(d, i)
            {
                if let Some(j) = (i + 2..n).find(|&j| {
                    chars[j] == d
                        && !used[j]
                        && !chars[j - 1].is_whitespace()
                        && closes(d, j + 1)
                }) {
                    push(out, base + i as i32, base + j as i32 + 1, "hl-emph");
                    mark(i, j + 1, &mut used);
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
    }
}

// --- JSON ---

fn json(text: &str) -> Vec<Span> {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = cs[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < n && cs[i] != '"' {
                i += if cs[i] == '\\' { 2 } else { 1 };
            }
            let close = i.min(n - 1);
            // A string is a key when the next non-space char is ':'.
            let mut k = close + 1;
            while k < n && cs[k].is_whitespace() {
                k += 1;
            }
            let tag = if k < n && cs[k] == ':' { "hl-key" } else { "hl-string" };
            push(&mut out, start as i32, (close + 1).min(n) as i32, tag);
            i = close + 1;
        } else if c == '-' || c.is_ascii_digit() {
            let prev_ident = i > 0 && (cs[i - 1].is_alphanumeric() || cs[i - 1] == '_');
            let start = i;
            i += 1;
            while i < n && matches!(cs[i], '0'..='9' | '.' | 'e' | 'E' | '+' | '-') {
                i += 1;
            }
            if !prev_ident && cs[start..i].iter().any(|c| c.is_ascii_digit()) {
                push(&mut out, start as i32, i as i32, "hl-number");
            }
        } else if c.is_ascii_alphabetic() {
            let start = i;
            while i < n && cs[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word: String = cs[start..i].iter().collect();
            if matches!(word.as_str(), "true" | "false" | "null") {
                push(&mut out, start as i32, i as i32, "hl-bool");
            }
        } else {
            i += 1;
        }
    }
    out
}

// --- YAML ---

/// Whether the quote at `i` opens a string. In YAML and TOML/INI (`strict`)
/// only one starting a value does; mid-word it is an apostrophe (`don't`,
/// `O'Brien`), which would otherwise swallow the rest of the line — comment
/// included. The shell quotes mid-word (`--opt="a b"`), so it isn't strict.
fn quote_opens(chars: &[char], i: usize, strict: bool) -> bool {
    !strict
        || i == 0
        || matches!(chars[i - 1], ' ' | '\t' | ':' | '=' | '[' | '{' | ',' | '(')
}

/// Position of the first '#' starting a comment (quote-aware, needs a
/// preceding space unless at line start), or line length.
fn comment_start(chars: &[char], hash_needs_space: bool, strict_quotes: bool) -> usize {
    let mut quote: Option<char> = None;
    for (i, &c) in chars.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' if quote_opens(chars, i, strict_quotes) => quote = Some(c),
                '#' => {
                    if !hash_needs_space || i == 0 || chars[i - 1].is_whitespace() {
                        return i;
                    }
                }
                _ => {}
            },
        }
    }
    chars.len()
}

/// Quoted string runs within `chars[from..to]`.
fn string_spans(chars: &[char], from: usize, to: usize, base: i32, strict: bool, out: &mut Vec<Span>) {
    let mut i = from;
    while i < to {
        let c = chars[i];
        if (c == '"' || c == '\'') && quote_opens(chars, i, strict) {
            if let Some(j) = (i + 1..to).find(|&j| chars[j] == c) {
                push(out, base + i as i32, base + j as i32 + 1, "hl-string");
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn numberish(token: &[char]) -> bool {
    !token.is_empty()
        && token.iter().any(|c| c.is_ascii_digit())
        && token
            .iter()
            .all(|c| matches!(c, '0'..='9' | '.' | '-' | '+' | ':' | '_' | 'e' | 'E' | 'x'))
}

/// The single trimmed value token in `chars[from..to]`, when there is
/// exactly one (start, end).
fn value_token(chars: &[char], from: usize, to: usize) -> Option<(usize, usize)> {
    let start = (from..to).find(|&i| !chars[i].is_whitespace())?;
    let mut end = to;
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    if chars[start..end].iter().any(|c| c.is_whitespace()) {
        return None;
    }
    Some((start, end))
}

fn classify_value(chars: &[char], from: usize, to: usize, base: i32, out: &mut Vec<Span>) {
    string_spans(chars, from, to, base, true, out);
    if let Some((start, end)) = value_token(chars, from, to) {
        let token: String = chars[start..end].iter().collect();
        let tag = match token.as_str() {
            "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off" => Some("hl-bool"),
            _ if numberish(&chars[start..end]) => Some("hl-number"),
            _ => None,
        };
        if let Some(tag) = tag {
            push(out, base + start as i32, base + end as i32, tag);
        }
    }
}

fn yaml(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    for (base, chars) in lines(text) {
        let n = chars.len();
        let code_end = comment_start(&chars, true, true);
        if code_end < n {
            push(&mut out, base + code_end as i32, base + n as i32, "hl-comment");
        }
        let trimmed: String = chars[..code_end].iter().collect::<String>().trim().to_string();
        if trimmed == "---" || trimmed == "..." {
            push(&mut out, base, base + code_end as i32, "hl-section");
            continue;
        }
        let mut i = chars.iter().take_while(|c| c.is_whitespace()).count();
        if i < code_end && chars[i] == '-' && (i + 1 == code_end || chars[i + 1] == ' ') {
            push(&mut out, base + i as i32, base + i as i32 + 1, "hl-marker");
            i += 1;
            while i < code_end && chars[i] == ' ' {
                i += 1;
            }
        }
        // key: — a colon followed by space or end-of-content.
        let key_end = (i..code_end).find(|&j| {
            chars[j] == ':' && (j + 1 >= code_end || chars[j + 1].is_whitespace())
        });
        match key_end {
            Some(j) if j > i => {
                push(&mut out, base + i as i32, base + j as i32, "hl-key");
                classify_value(&chars, j + 1, code_end, base, &mut out);
            }
            _ => classify_value(&chars, i, code_end, base, &mut out),
        }
    }
    out
}

// --- TOML ---

fn toml(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    for (base, chars) in lines(text) {
        let n = chars.len();
        let code_end = comment_start(&chars, false, true);
        if code_end < n {
            push(&mut out, base + code_end as i32, base + n as i32, "hl-comment");
        }
        let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
        if indent >= code_end {
            continue;
        }
        let mut content_end = code_end;
        while content_end > indent && chars[content_end - 1].is_whitespace() {
            content_end -= 1;
        }
        if chars[indent] == '[' && chars[content_end - 1] == ']' {
            push(&mut out, base + indent as i32, base + content_end as i32, "hl-section");
            continue;
        }
        // key = value; '=' found outside quotes.
        let mut quote: Option<char> = None;
        let eq = (indent..code_end).find(|&i| {
            let c = chars[i];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                    false
                }
                None => match c {
                    '"' | '\'' => {
                        quote = Some(c);
                        false
                    }
                    '=' => true,
                    _ => false,
                },
            }
        });
        if let Some(eq) = eq {
            let mut key_end = eq;
            while key_end > indent && chars[key_end - 1].is_whitespace() {
                key_end -= 1;
            }
            push(&mut out, base + indent as i32, base + key_end as i32, "hl-key");
            classify_value(&chars, eq + 1, code_end, base, &mut out);
        } else {
            string_spans(&chars, indent, code_end, base, true, &mut out);
        }
    }
    out
}

// --- Shell ---

/// Words that carry the shape of a script rather than doing work. Commands
/// (`echo`, `grep`, …) are deliberately absent: there is no fixed list of
/// them, and highlighting a guessed few reads as noise.
const SHELL_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "select", "function", "return", "local", "export", "readonly", "declare", "typeset",
    "shift", "break", "continue", "exit", "source", "set", "unset", "trap", "eval", "exec",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn shell(text: &str) -> Vec<Span> {
    let mut out = Vec::new();
    for (line_no, (base, chars)) in lines(text).enumerate() {
        let n = chars.len();
        // The shebang is a comment to the shell but the most load-bearing
        // line in the file; give it the weight the eye expects.
        if line_no == 0 && chars.starts_with(&['#', '!']) {
            push(&mut out, base, base + n as i32, "hl-section");
            continue;
        }
        // Same rule as YAML: a '#' at line start or after whitespace, and
        // not inside quotes — which keeps ${v#pat}, $# and "a # b" intact.
        let code_end = comment_start(&chars, true, false);
        if code_end < n {
            push(&mut out, base + code_end as i32, base + n as i32, "hl-comment");
        }
        let mut i = 0;
        while i < code_end {
            let c = chars[i];
            // $name, ${...}, $1, $?, $@ — the parameter, not the expansion,
            // so $( … ) is left to be highlighted by what is inside it.
            if c == '$' && i + 1 < code_end && chars[i + 1] != '(' {
                let start = i;
                i += 1;
                if chars[i] == '{' {
                    i = (i + 1..code_end).find(|&j| chars[j] == '}').map_or(code_end, |j| j + 1);
                } else if is_word(chars[i]) {
                    while i < code_end && is_word(chars[i]) {
                        i += 1;
                    }
                } else if matches!(chars[i], '?' | '@' | '*' | '#' | '!' | '$' | '-') {
                    i += 1;
                }
                push(&mut out, base + start as i32, base + i as i32, "hl-key");
                continue;
            }
            if is_word(c) && (i == 0 || !is_word(chars[i - 1]) && chars[i - 1] != '$') {
                let start = i;
                while i < code_end && is_word(chars[i]) {
                    i += 1;
                }
                // Only a word standing on its own is a keyword: one glued to
                // a path or flag (`/usr/local`, `--local`, `-exec`) is not.
                let free = start == 0
                    || matches!(chars[start - 1], ' ' | '\t' | ';' | '|' | '&' | '(' | '{' | '`' | '!');
                if !free {
                    continue;
                }
                let word: String = chars[start..i].iter().collect();
                // name() — a function definition, not a call.
                let paren = (i..code_end).take_while(|&j| chars[j] == ' ').count() + i;
                if chars.get(paren) == Some(&'(') && chars.get(paren + 1) == Some(&')') {
                    push(&mut out, base + start as i32, base + i as i32, "hl-section");
                } else if SHELL_WORDS.contains(&word.as_str()) {
                    push(&mut out, base + start as i32, base + i as i32, "hl-bool");
                }
                continue;
            }
            i += 1;
        }
        // Quoted runs overlap the variables inside them; hl-string is added
        // to the tag table after hl-key, so GTK's tag priority already gives
        // the quote the last word and the two can simply both be emitted.
        string_spans(&chars, 0, code_end, base, false, &mut out);
    }
    out
}

// --- XML ---

fn at(cs: &[char], i: usize, pat: &str) -> bool {
    pat.chars().enumerate().all(|(k, c)| cs.get(i + k) == Some(&c))
}

fn find_from(cs: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..cs.len()).find(|&i| at(cs, i, pat))
}

/// Markup only: element names, attribute names and values, comments and the
/// prolog. Character data is left alone — it is the part a reader is reading.
fn xml(text: &str) -> Vec<Span> {
    let cs: Vec<char> = text.chars().collect();
    let n = cs.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if cs[i] != '<' {
            i += 1;
            continue;
        }
        let start = i;
        if at(&cs, i, "<!--") {
            let end = find_from(&cs, i + 4, "-->").map_or(n, |j| j + 3);
            push(&mut out, start as i32, end as i32, "hl-comment");
            i = end;
            continue;
        }
        if at(&cs, i, "<![CDATA[") {
            let end = find_from(&cs, i + 9, "]]>").map_or(n, |j| j + 3);
            // Bracket the raw text rather than colouring it: the point of
            // CDATA is that its content is not markup.
            push(&mut out, start as i32, start as i32 + 9, "hl-marker");
            push(&mut out, (end as i32 - 3).max(start as i32 + 9), end as i32, "hl-marker");
            i = end;
            continue;
        }
        // <?xml … ?> and <!DOCTYPE …>.
        if at(&cs, i, "<?") || at(&cs, i, "<!") {
            let end = (i..n).find(|&j| cs[j] == '>').map_or(n, |j| j + 1);
            push(&mut out, start as i32, end as i32, "hl-marker");
            i = end;
            continue;
        }
        let mut j = i + 1;
        if j < n && cs[j] == '/' {
            j += 1;
        }
        let name_start = j;
        while j < n && (cs[j].is_alphanumeric() || matches!(cs[j], '_' | '-' | '.' | ':')) {
            j += 1;
        }
        if j == name_start {
            // A bare '<' in text ("a < b"), not the start of a tag.
            i += 1;
            continue;
        }
        push(&mut out, name_start as i32, j as i32, "hl-section");
        // Attributes, up to the '>' that is not inside a quoted value.
        while j < n && cs[j] != '>' {
            let c = cs[j];
            if c == '"' || c == '\'' {
                let close = (j + 1..n).find(|&m| cs[m] == c).unwrap_or(n - 1);
                push(&mut out, j as i32, (close + 1).min(n) as i32, "hl-string");
                j = close + 1;
            } else if c.is_alphabetic() || c == '_' {
                let a = j;
                while j < n && (cs[j].is_alphanumeric() || matches!(cs[j], '_' | '-' | '.' | ':')) {
                    j += 1;
                }
                push(&mut out, a as i32, j as i32, "hl-key");
            } else {
                j += 1;
            }
        }
        i = (j + 1).min(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The (tag, text) pairs of all spans, for readable assertions.
    fn tagged(text: &str, kind: DocKind) -> Vec<(String, String)> {
        let chars: Vec<char> = text.chars().collect();
        spans(text, kind)
            .iter()
            .map(|s| {
                let run: String =
                    chars[s.start as usize..s.end as usize].iter().collect();
                (s.tag.to_string(), run)
            })
            .collect()
    }

    fn has(spans: &[(String, String)], tag: &str, text: &str) -> bool {
        spans.iter().any(|(t, s)| t == tag && s == text)
    }

    #[test]
    fn markdown_spans() {
        let text = "# Title\n\n- item with `code` and **bold** and *it*\n> quote\n```\nraw\n```\nsee [x](http://y)\n";
        let s = tagged(text, DocKind::Markdown);
        assert!(has(&s, "hl-heading", "# Title"));
        assert!(has(&s, "hl-marker", "-"));
        assert!(has(&s, "hl-code", "`code`"));
        assert!(has(&s, "hl-strong", "**bold**"));
        assert!(has(&s, "hl-emph", "*it*"));
        assert!(has(&s, "hl-marker", ">"));
        assert!(has(&s, "hl-code", "raw"));
        assert!(has(&s, "hl-link", "[x]"));
        assert!(has(&s, "hl-url", "(http://y)"));
    }

    #[test]
    fn markdown_task_and_numbered_markers() {
        let s = tagged("1. one\n- [x] done\n", DocKind::Markdown);
        assert!(has(&s, "hl-marker", "1."));
        assert!(has(&s, "hl-marker", "- [x]"));
    }

    #[test]
    fn json_spans() {
        let s = tagged(
            "{\"name\": \"bindle\", \"port\": 8080, \"on\": true, \"x\": null}",
            DocKind::Json,
        );
        assert!(has(&s, "hl-key", "\"name\""));
        assert!(has(&s, "hl-string", "\"bindle\""));
        assert!(has(&s, "hl-number", "8080"));
        assert!(has(&s, "hl-bool", "true"));
        assert!(has(&s, "hl-bool", "null"));
    }

    #[test]
    fn yaml_spans() {
        let text = "---\nname: bindle # the app\nport: 8080\nflags:\n  - enabled: true\n";
        let s = tagged(text, DocKind::Yaml);
        assert!(has(&s, "hl-section", "---"));
        assert!(has(&s, "hl-key", "name"));
        assert!(has(&s, "hl-comment", "# the app"));
        assert!(has(&s, "hl-number", "8080"));
        assert!(has(&s, "hl-marker", "-"));
        assert!(has(&s, "hl-key", "enabled"));
        assert!(has(&s, "hl-bool", "true"));
    }

    #[test]
    fn toml_spans() {
        let text = "# config\n[server]\nhost = \"localhost\"\nport = 8080\ndebug = false\n";
        let s = tagged(text, DocKind::Toml);
        assert!(has(&s, "hl-comment", "# config"));
        assert!(has(&s, "hl-section", "[server]"));
        assert!(has(&s, "hl-key", "host"));
        assert!(has(&s, "hl-string", "\"localhost\""));
        assert!(has(&s, "hl-number", "8080"));
        assert!(has(&s, "hl-bool", "false"));
    }

    #[test]
    fn plain_kinds_have_no_spans() {
        assert!(spans("# not a heading", DocKind::Text).is_empty());
        assert!(spans("╔═╗", DocKind::Nfo).is_empty());
    }

    #[test]
    fn shell_spans() {
        let text = "#!/bin/bash\n# setup\nbuild() {\n  local dir=\"$HOME/x\"\n  if [ -d \"$dir\" ]; then\n    export N=${COUNT}\n  fi\n}\n";
        let s = tagged(text, DocKind::Shell);
        assert!(has(&s, "hl-section", "#!/bin/bash"));
        assert!(has(&s, "hl-comment", "# setup"));
        assert!(has(&s, "hl-section", "build"));
        assert!(has(&s, "hl-bool", "local"));
        assert!(has(&s, "hl-bool", "if"));
        assert!(has(&s, "hl-bool", "then"));
        assert!(has(&s, "hl-bool", "export"));
        assert!(has(&s, "hl-key", "$HOME"));
        assert!(has(&s, "hl-key", "${COUNT}"));
        assert!(has(&s, "hl-string", "\"$HOME/x\""));
    }

    #[test]
    fn shell_hash_is_only_a_comment_after_space() {
        // ${v#pat} and $# are parameter syntax, not the start of a comment.
        let s = tagged("v=${p#/a} n=$#  # real\n", DocKind::Shell);
        assert!(has(&s, "hl-comment", "# real"));
        assert!(!s.iter().any(|(t, run)| t == "hl-comment" && run.contains("${")));
    }

    #[test]
    fn xml_spans() {
        let text = "<?xml version=\"1.0\"?>\n<!-- note -->\n<doc id=\"7\">\n  <a href='x'>text &amp; more</a>\n</doc>\n";
        let s = tagged(text, DocKind::Xml);
        assert!(has(&s, "hl-marker", "<?xml version=\"1.0\"?>"));
        assert!(has(&s, "hl-comment", "<!-- note -->"));
        assert!(has(&s, "hl-section", "doc"));
        assert!(has(&s, "hl-key", "id"));
        assert!(has(&s, "hl-string", "\"7\""));
        assert!(has(&s, "hl-string", "'x'"));
        // Character data is never tagged.
        assert!(!s.iter().any(|(_, run)| run.contains("text &amp;")));
    }

    #[test]
    fn xml_bare_less_than_is_not_a_tag() {
        let s = tagged("<p>a < b</p>\n", DocKind::Xml);
        assert_eq!(s.iter().filter(|(t, _)| t == "hl-section").count(), 2);
    }

    #[test]
    fn no_span_crosses_a_newline() {
        // An unterminated comment runs to end of file; it must still come
        // back as one span per line, or line_sigs would misattribute it.
        let text = "<a/>\n<!-- open\nstill open\nand open\n";
        let out = spans(text, DocKind::Xml);
        let nl: Vec<i32> = newline_offsets(text);
        for s in &out {
            assert!(
                !nl.iter().any(|n| s.start <= *n && *n < s.end),
                "span {}..{} crosses a newline",
                s.start,
                s.end
            );
        }
        assert!(out.iter().filter(|s| s.tag == "hl-comment").count() >= 3);
    }

    /// Signatures for `text`, as `apply_highlight` computes them.
    fn sigs_of(text: &str, kind: DocKind) -> Vec<u64> {
        line_sigs(text, &spans(text, kind)).1
    }

    #[test]
    fn editing_one_line_dirties_only_that_line() {
        let before = "# Title\n\nbody **one**\nbody two\nbody three\n";
        let after = "# Title\n\nbody **one**\nbody TWO\nbody three\n";
        let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
        assert_eq!(dirty_lines(&a, &b), Some((3, 3)));
    }

    #[test]
    fn inserting_a_line_dirties_only_the_new_one() {
        let before = "a\nb\nc\n";
        let after = "a\nNEW\nb\nc\n";
        let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
        assert_eq!(dirty_lines(&a, &b), Some((1, 1)));
    }

    #[test]
    fn retyped_identical_text_is_not_dirty_at_all() {
        let text = "# T\n\n- a\n- b\n";
        let s = sigs_of(text, DocKind::Markdown);
        assert_eq!(dirty_lines(&s, &s), None);
    }

    #[test]
    fn opening_a_fence_dirties_every_line_below_it() {
        // The text of lines 2-4 is untouched, but they are now code. The
        // signature has to notice that, or they keep their old tags.
        let before = "intro\nplain\nmore plain\nlast\n";
        let after = "intro\n```\nmore plain\nlast\n";
        let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
        let (lo, hi) = dirty_lines(&a, &b).expect("fence restyles the rest");
        assert_eq!(lo, 1);
        assert!(hi >= 3, "lines below the fence must be re-tagged, got {hi}");
    }

    #[test]
    fn deleting_trailing_lines_is_not_dirty() {
        for (before, after) in [("hello\n", "hello"), ("a\nb\nc", "a"), ("# T\n\n", "# T")] {
            let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
            assert_eq!(dirty_lines(&a, &b), None, "{before:?} -> {after:?}");
        }
    }

    #[test]
    fn dirty_range_always_indexes_the_new_lines() {
        // `apply_highlight` indexes line starts with the result, so any range
        // outside the new document is a crash, not just wasted work.
        let texts = ["", "a", "a\n", "a\nb", "a\nb\n", "\n\n", "# T\n\n- x\n", "```\nx\n", "a\n```\n"];
        for before in texts {
            for after in texts {
                let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
                if let Some((lo, hi)) = dirty_lines(&a, &b) {
                    assert!(lo <= hi && hi < b.len(), "{before:?} -> {after:?}: ({lo}, {hi})");
                }
            }
        }
    }

    #[test]
    fn a_line_joined_by_a_multiline_delete_is_retagged() {
        // "a", fence, "x"(code), fence, "x"(plain). Deleting lines 1-2 whole
        // leaves the plain "x" under a fence that now opens — code. By
        // position its signature equals the old line 2 ("x", code), so a
        // positional comparison found nothing to do and it stayed plain.
        let before = "a\n```\nx\n```\nx";
        let after = "a\n```\nx";
        let mut baseline = sigs_of(before, DocKind::Markdown);
        // Deleting from the start of line 1 to the start of line 3.
        note_delete(&mut baseline, 1, 3);
        let new = sigs_of(after, DocKind::Markdown);
        assert_eq!(baseline.len(), new.len());
        let (lo, hi) = dirty_lines(&baseline, &new).expect("edit is dirty");
        assert!(lo <= 1 && hi >= 2, "line 2 must be retagged, got ({lo}, {hi})");
    }

    #[test]
    fn tracking_follows_line_counts() {
        let mut baseline = vec![10, 11, 12];
        note_insert(&mut baseline, 1, 2);
        assert_eq!(baseline, [10, EDITED, EDITED, EDITED, 12]);
        note_delete(&mut baseline, 0, 3);
        assert_eq!(baseline, [EDITED, 12]);
        // An empty baseline (full pass pending) stays empty.
        let mut empty = Vec::new();
        note_insert(&mut empty, 0, 1);
        assert!(empty.is_empty());
    }

    #[test]
    fn a_line_that_only_moved_is_not_dirty() {
        // Line "**b**" shifts down by one line; its tags travel with the
        // text in the buffer, so its signature must not change.
        let before = "a\n**b**\n";
        let after = "a\nNEW\n**b**\n";
        let (a, b) = (sigs_of(before, DocKind::Markdown), sigs_of(after, DocKind::Markdown));
        assert_eq!(a[1], b[2]);
    }

    #[test]
    fn an_apostrophe_does_not_hide_a_yaml_or_toml_comment() {
        let s = tagged("msg: don't panic # note\n", DocKind::Yaml);
        assert!(has(&s, "hl-comment", "# note"));
        let s = tagged("name = O'Brien # who\n", DocKind::Toml);
        assert!(has(&s, "hl-comment", "# who"));
        // A quote that does open a value still protects its '#'.
        let s = tagged("k: 'a # b'\n", DocKind::Yaml);
        assert!(has(&s, "hl-string", "'a # b'"));
        assert!(!s.iter().any(|(t, _)| t == "hl-comment"));
    }

    #[test]
    fn shell_keywords_inside_paths_and_flags_are_not_keywords() {
        let s = tagged("cd /usr/local; git config --local x; find . -exec rm {} \\;\n", DocKind::Shell);
        assert!(!s.iter().any(|(t, _)| t == "hl-bool"), "{s:?}");
        let s = tagged("x=1; local y; if true; then :; fi\n", DocKind::Shell);
        assert!(has(&s, "hl-bool", "local"));
        assert!(has(&s, "hl-bool", "then"));
    }

    #[test]
    fn underscores_inside_words_are_not_emphasis() {
        let s = tagged("call snake_case_name and __init__ here, _real_ too\n", DocKind::Markdown);
        assert!(!has(&s, "hl-emph", "_case_"));
        assert!(has(&s, "hl-emph", "_real_"));
        assert!(has(&s, "hl-strong", "__init__"));
        assert!(has(&tagged("x*a*y\n", DocKind::Markdown), "hl-emph", "*a*"));
    }

    #[test]
    fn a_fence_closes_only_on_its_own_kind() {
        let s = tagged("~~~\n```\ncode\n~~~\nplain\n", DocKind::Markdown);
        assert!(has(&s, "hl-code", "code"));
        assert!(!has(&s, "hl-code", "plain"));
        // "```rust" can't close a fence: it has an info string.
        let s = tagged("```\n```rust\nstill\n```\nafter\n", DocKind::Markdown);
        assert!(has(&s, "hl-code", "still"));
        assert!(!has(&s, "hl-code", "after"));
    }

    #[test]
    fn indented_hashes_are_not_a_heading() {
        assert!(!has(&tagged("    # code\n", DocKind::Markdown), "hl-heading", "    # code"));
        assert!(has(&tagged("  # still\n", DocKind::Markdown), "hl-heading", "  # still"));
    }

    #[test]
    fn lines_break_where_gtk_breaks_them() {
        let text = "a\rb\r\nc\u{2029}d\n";
        let (starts, sigs) = line_sigs(text, &[]);
        assert_eq!(starts, [0, 2, 5, 7, 9]);
        assert_eq!(sigs.len(), 5);
        assert_eq!(line_breaks(text), 4);
        assert_eq!(line_breaks("\r\n\r\n"), 2);
    }

    #[test]
    fn a_span_starting_on_a_break_is_split() {
        let out = split_at_newlines("ab\ncd", vec![Span { start: 2, end: 5, tag: "hl-code" }]);
        assert!(out.iter().all(|s| s.start >= 3 || s.end <= 2));
        assert!(out.iter().any(|s| s.start == 3 && s.end == 5));
    }
}
