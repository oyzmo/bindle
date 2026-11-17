//! Pure decision logic for smart Enter/Backspace in the editor.
//! Kept free of GTK types so it can be unit-tested directly.

pub enum EnterAction {
    /// Do nothing special; let the TextView insert a plain newline.
    None,
    /// Insert this text at the cursor (newline + next marker).
    Continue(String),
    /// Insert `text`, then move the cursor `cursor_back` characters left
    /// (used to land inside the first cell of a fresh table row).
    ContinueTable { text: String, cursor_back: i32 },
    /// The line is an empty list marker: delete from line start to cursor.
    StopEmpty,
    /// The line is an empty table row: clear the whole line, including
    /// the cells after the cursor.
    ClearLine,
}

/// What Backspace should do instead of deleting one character.
#[derive(Debug, PartialEq)]
pub enum BackspaceAction {
    /// Nothing special: let the TextView delete a character.
    None,
    /// Delete from line start to the cursor (an empty list marker).
    ClearBefore,
    /// Clear the whole line (an empty table row).
    ClearLine,
}

struct Marker {
    indent: String,
    next: String,
    /// Byte length of the marker (indent included) in the source line.
    len: usize,
}

/// Recognize a list marker at the start of a line: `- `, `* `, `+ `,
/// `- [ ] ` / `- [x] ` task items, or `12. ` numbered items.
fn parse_marker(line: &str) -> Option<Marker> {
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let rest = &line[indent.len()..];

    if let Some(bullet) = rest.chars().next().filter(|c| "-*+".contains(*c)) {
        let after = &rest[bullet.len_utf8()..];
        if let Some(task) = after.strip_prefix(' ').and_then(|a| {
            ["[ ] ", "[x] ", "[X] "].iter().find(|t| a.starts_with(*t))
        }) {
            return Some(Marker {
                len: indent.len() + 2 + task.len(),
                next: format!("{bullet} [ ] "),
                indent,
            });
        }
        if after.starts_with(' ') {
            return Some(Marker {
                len: indent.len() + bullet.len_utf8() + 1,
                next: format!("{bullet} "),
                indent,
            });
        }
    }

    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        if let Some(after) = rest[digits.len()..].strip_prefix(". ") {
            let _ = after;
            let n: u64 = digits.parse().unwrap_or(0);
            return Some(Marker {
                len: indent.len() + digits.len() + 2,
                next: format!("{}. ", n.saturating_add(1)),
                indent,
            });
        }
    }
    None
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_empty_table_row(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '|' || c == ' ' || c == '\t')
}

/// Decide what Enter should do, given the current line's text before and
/// after the cursor. Lists go by what precedes the cursor; a table row by
/// the whole line — a fresh row puts the cursor in its first cell, and the
/// empty cells after it are part of the row.
pub fn on_enter(before_cursor: &str, after_cursor: &str) -> EnterAction {
    if let Some(marker) = parse_marker(before_cursor) {
        if before_cursor[marker.len..].trim().is_empty() {
            return EnterAction::StopEmpty;
        }
        return EnterAction::Continue(format!("\n{}{}", marker.indent, marker.next));
    }
    if is_table_row(before_cursor) {
        let line = format!("{before_cursor}{after_cursor}");
        if is_empty_table_row(&line) {
            return EnterAction::ClearLine;
        }
        let pipes = line.chars().filter(|c| *c == '|').count();
        if pipes >= 2 {
            // "|  |  |" with one cell per column; cursor lands after "| ".
            let row: String = std::iter::once("|".to_string())
                .chain((0..pipes - 1).map(|_| "  |".to_string()))
                .collect();
            let cursor_back = row.chars().count() as i32 - 2;
            return EnterAction::ContinueTable { text: format!("\n{row}"), cursor_back };
        }
    }
    EnterAction::None
}

/// Whether Backspace should wipe an empty list marker (line start to
/// cursor) or an empty table row (the whole line, as for Enter).
pub fn on_backspace(before_cursor: &str, after_cursor: &str) -> BackspaceAction {
    if let Some(marker) = parse_marker(before_cursor) {
        return if before_cursor[marker.len..].trim().is_empty() {
            BackspaceAction::ClearBefore
        } else {
            BackspaceAction::None
        };
    }
    if is_empty_table_row(before_cursor) && is_empty_table_row(&format!("{before_cursor}{after_cursor}")) {
        return BackspaceAction::ClearLine;
    }
    BackspaceAction::None
}

/// Deepest ATX heading level (CommonMark).
pub const MAX_HEADING: u8 = 6;

/// A heading in the source, found by the markdown parser (so `#` lines
/// inside code blocks and `#hashtags` are never mistaken for one).
#[derive(Debug, PartialEq)]
pub struct Heading {
    pub level: u8,
    /// Char offset of the first `#`, or None for a setext heading
    /// (underlined with `===`/`---`), which has no `#` run to change.
    pub hash: Option<usize>,
}

pub fn headings(text: &str) -> Vec<Heading> {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut found = Vec::new();
    // Byte → char offsets, walked forward once since events come in order.
    let (mut byte, mut chars) = (0usize, 0usize);
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        let Event::Start(Tag::Heading { level, .. }) = event else { continue };
        let rest = &text[range.start..];
        let skipped = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        let start = range.start + skipped;
        let hash = text[start..].starts_with('#').then(|| {
            chars += text[byte..start].chars().count();
            byte = start;
            chars
        });
        found.push(Heading { level: level as u8, hash });
    }
    found
}

/// Whether every heading can move one level up (`delta` -1, one `#`
/// fewer) or down (+1) without leaving 1..=MAX_HEADING. All or nothing,
/// so the hierarchy is never flattened and −/+ always round-trip.
/// Setext headings count toward the bounds even though they don't move.
pub fn can_shift(headings: &[Heading], delta: i8) -> bool {
    headings.iter().any(|h| h.hash.is_some())
        && headings.iter().all(|h| {
            let level = h.level as i8 + delta;
            (1..=MAX_HEADING as i8).contains(&level)
        })
}

/// Char offsets where one `#` is inserted (delta +1) or deleted (−1),
/// in document order; None when `can_shift` says no.
pub fn shift_offsets(headings: &[Heading], delta: i8) -> Option<Vec<usize>> {
    can_shift(headings, delta).then(|| headings.iter().filter_map(|h| h.hash).collect())
}

/// Collapsed-section key (`level:occurrence:text`, see preview.rs) after
/// a shift by `delta`; None for a key that isn't one.
pub fn shift_section_key(key: &str, delta: i8) -> Option<String> {
    let (level, rest) = key.split_once(':')?;
    let level = level.parse::<i8>().ok()? + delta;
    Some(format!("{level}:{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn continues(line: &str) -> String {
        match on_enter(line, "") {
            EnterAction::Continue(s) => s,
            _ => panic!("expected Continue for {line:?}"),
        }
    }

    #[test]
    fn bullet_continues() {
        assert_eq!(continues("- item"), "\n- ");
        assert_eq!(continues("* item"), "\n* ");
        assert_eq!(continues("  - nested"), "\n  - ");
    }

    #[test]
    fn task_continues_unchecked() {
        assert_eq!(continues("- [x] done"), "\n- [ ] ");
        assert_eq!(continues("- [ ] todo"), "\n- [ ] ");
    }

    #[test]
    fn number_increments() {
        assert_eq!(continues("1. one"), "\n2. ");
        assert_eq!(continues("  9. nine"), "\n  10. ");
    }

    #[test]
    fn table_continues_with_matching_columns() {
        match on_enter("| a | b |", "") {
            EnterAction::ContinueTable { text, cursor_back } => {
                assert_eq!(text, "\n|  |  |");
                assert_eq!(cursor_back, 5);
            }
            _ => panic!("expected table continuation"),
        }
    }

    #[test]
    fn empty_markers_stop() {
        assert!(matches!(on_enter("- ", ""), EnterAction::StopEmpty));
        assert!(matches!(on_enter("- [ ] ", ""), EnterAction::StopEmpty));
        assert!(matches!(on_enter("3. ", ""), EnterAction::StopEmpty));
        assert!(matches!(on_enter("|  |  |", ""), EnterAction::ClearLine));
    }

    #[test]
    fn a_fresh_table_row_is_cleared_whole() {
        // Where Enter on a row leaves the cursor: in the first cell, with the
        // empty cells after it. Clearing only "| " left "  |  |" behind.
        assert!(matches!(on_enter("| ", " |  |"), EnterAction::ClearLine));
        assert_eq!(on_backspace("| ", " |  |"), BackspaceAction::ClearLine);
        // A row with text after the cursor is not empty.
        assert!(matches!(on_enter("| ", " | b |"), EnterAction::ContinueTable { .. }));
        assert_eq!(on_backspace("| ", "a | b |"), BackspaceAction::None);
    }

    #[test]
    fn plain_text_is_ignored() {
        assert!(matches!(on_enter("hello", ""), EnterAction::None));
        assert!(matches!(on_enter("", ""), EnterAction::None));
        assert!(matches!(on_enter("-not a list", ""), EnterAction::None));
        assert!(matches!(on_enter("1.no space", ""), EnterAction::None));
    }

    #[test]
    fn backspace_clears_empty_markers_only() {
        assert_eq!(on_backspace("- ", ""), BackspaceAction::ClearBefore);
        assert_eq!(on_backspace("  2. ", ""), BackspaceAction::ClearBefore);
        assert_eq!(on_backspace("| | |", ""), BackspaceAction::ClearLine);
        assert_eq!(on_backspace("- x", ""), BackspaceAction::None);
        assert_eq!(on_backspace("plain", ""), BackspaceAction::None);
    }

    fn shifted(text: &str, delta: i8) -> Option<String> {
        let offsets = shift_offsets(&headings(text), delta)?;
        let mut chars: Vec<char> = text.chars().collect();
        for &at in offsets.iter().rev() {
            if delta > 0 { chars.insert(at, '#') } else { assert_eq!(chars.remove(at), '#') }
        }
        Some(chars.into_iter().collect())
    }

    #[test]
    fn headings_shift_together() {
        let doc = "# Æøå\n\ntext #not\n\n## Two ##\n   ### Three\n> #### Quoted\n";
        assert_eq!(
            shifted(doc, 1).unwrap(),
            "## Æøå\n\ntext #not\n\n### Two ##\n   #### Three\n> ##### Quoted\n"
        );
        assert_eq!(shifted(&shifted(doc, 1).unwrap(), -1).unwrap(), doc);
    }

    #[test]
    fn bounds_are_all_or_nothing() {
        assert_eq!(shifted("# One\n## Two\n", -1), None);
        assert_eq!(shifted("## One\n###### Six\n", 1), None);
        assert_eq!(shifted("## One\n### Two\n", -1).unwrap(), "# One\n## Two\n");
    }

    #[test]
    fn code_hashtags_and_setext_are_left_alone() {
        let doc = "```sh\n# comment\n```\n#hashtag\n\nTitle\n-----\n\n### Real\n";
        let hs = headings(doc);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0], Heading { level: 2, hash: None });
        assert_eq!(shifted(doc, 1).unwrap(), doc.replace("### Real", "#### Real"));
        // A setext H1 is the floor for raising.
        assert_eq!(shifted("Title\n===\n\n## Two\n", -1), None);
        // Nothing to move at all.
        assert_eq!(shifted("Title\n===\n", 1), None);
        assert_eq!(shifted("plain\n", 1), None);
    }

    #[test]
    fn section_keys_follow_the_shift() {
        assert_eq!(shift_section_key("2:0:A: b", 1).as_deref(), Some("3:0:A: b"));
        assert_eq!(shift_section_key("bogus", 1), None);
    }
}
