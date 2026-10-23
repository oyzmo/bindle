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
}
