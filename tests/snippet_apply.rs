// Verifies snippet insertion behavior against a real TextBuffer.
// Needs a working GTK display; skipped automatically when none is available.

use gtk4::prelude::*;
use bindle::{editor, snippets};

fn find(label: &str) -> &'static snippets::Snippet {
    snippets::all().iter().find(|s| s.label == label).unwrap()
}

#[test]
fn table_leaves_cursor_on_last_row() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }

    let buffer = gtk4::TextBuffer::new(None);
    snippets::apply(&buffer, find("Table"));

    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    assert!(cursor.ends_line(), "cursor should be at end of a line");
    assert_eq!(cursor.line(), 2, "cursor should be on the table's last row");

    // Pressing Enter here must continue the table with a fresh row.
    let mut line_start = cursor.clone();
    line_start.set_line_offset(0);
    let before = buffer.text(&line_start, &cursor, false).to_string();
    assert!(matches!(
        editor::on_enter(&before),
        editor::EnterAction::ContinueTable { .. }
    ));

    // Bullet and task list markers stay distinct.
    buffer.set_text("");
    snippets::apply(&buffer, find("Bullet list"));
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    assert_eq!(text.as_str(), "- ");

    buffer.set_text("");
    snippets::apply(&buffer, find("Task list"));
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    assert_eq!(text.as_str(), "- [ ] ");

    // (Kept in this one test: a second GTK test in the file would race it
    // for `gtk4::init` — see the CLAUDE.md note on preview_render.)
    // A line-wise selection ends at the start of the next line; that line
    // was not selected and must not be prefixed.
    buffer.set_text("one\ntwo\nthree");
    let (start, end) = (buffer.iter_at_line(0).unwrap(), buffer.iter_at_line(2).unwrap());
    buffer.select_range(&start, &end);
    snippets::apply(&buffer, find("Bullet list"));
    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
    assert_eq!(text.as_str(), "- one\n- two\nthree");
}

#[test]
fn web_image_markdown_builds_sensible_alt() {
    let (md, alt) =
        snippets::web_image_markdown("https://x.org/pics/red-hat_photo.jpg?w=1200").unwrap();
    assert_eq!(alt, "red hat photo");
    assert_eq!(md, "![red hat photo](https://x.org/pics/red-hat_photo.jpg?w=1200)");

    // Missing scheme gets https://, URLs without a file name fall back.
    let (md, alt) = snippets::web_image_markdown("example.com/a.png").unwrap();
    assert_eq!(alt, "a");
    assert_eq!(md, "![a](https://example.com/a.png)");
    let (_, alt) = snippets::web_image_markdown("https://example.com/").unwrap();
    assert_eq!(alt, "image");

    assert!(snippets::web_image_markdown("   ").is_none());
}
