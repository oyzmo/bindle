// Renders sample markdown into a TextBuffer and checks the visible text.
// Needs a working GTK display; skipped automatically when none is available.

use gtk4::prelude::*;
use bindle::preview;

#[test]
fn renders_markdown_structure() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }

    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);

    let markdown = "\
# Title

Some *italic* and **bold** text with `code`.

- first
- second
  - nested

1. one
2. two

> a quote

```rust
let x = 1;
```

| A | B |
| - | - |
| 1 | 2 |

- [x] done
- [ ] todo
";
    let rendered = preview::render(&buffer, markdown, None, false, None, None);
    assert!(!rendered.saw_remote, "document has no remote images");
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();

    assert!(text.contains("Title"));
    assert!(text.contains("Some italic and bold text with code."));
    assert!(text.contains("• first"));
    assert!(text.contains("    • nested"));
    assert!(text.contains("1. one"));
    assert!(text.contains("2. two"));
    assert!(text.contains("a quote"));
    assert!(text.contains("let x = 1;"));
    assert!(text.contains("A │ B"));
    assert!(text.contains("──┼──"));
    assert!(text.contains("1 │ 2"));
    assert!(text.contains("☑ done"));
    assert!(text.contains("☐ todo"));

    // Blocks are separated by blank lines, with no leading whitespace at the top.
    assert!(!text.starts_with('\n'));
    assert!(text.contains("Title\n\nSome"));

    // Source map: a preview position inside a text run maps back to the
    // corresponding character in the markdown source.
    let map = &rendered.source_map;
    let preview_chars: Vec<char> = text.chars().collect();
    let source_chars: Vec<char> = markdown.chars().collect();
    for needle in ["Title", "italic", "bold", "first", "nested", "quote"] {
        let p = find_char_offset(&preview_chars, needle);
        // Click one char into the word: source position must sit on the
        // same character of the same word.
        let src = map.source_offset(p + 1).expect("mapped") as usize;
        assert_eq!(
            source_chars[src],
            needle.chars().nth(1).unwrap(),
            "{needle}: mapped to wrong source char"
        );
    }
    // Reverse lookup: a source position maps back into the preview at the
    // same character of the same word (keeps the preview centered on the
    // spot being edited).
    for needle in ["Title", "italic", "nested", "quote"] {
        let s = find_char_offset(&source_chars, needle);
        let p = map.preview_offset(s + 1).expect("mapped");
        assert_eq!(
            preview_chars[p as usize],
            needle.chars().nth(1).unwrap(),
            "{needle}: reverse-mapped to wrong preview char"
        );
    }

    // A bullet marker (non-exact run) anchors to its list item's source.
    let p = find_char_offset(&preview_chars, "• first");
    let src = map.source_offset(p).expect("mapped") as usize;
    assert_eq!(&source_chars[src..src + 2].iter().collect::<String>(), "- ");

    // Remote images: reported, but only a placeholder when not allowed
    // (nothing is fetched — the alt text shows with the 🖼 marker).
    let rendered = preview::render(
        &buffer,
        "![webpic](https://example.org/pic.png)",
        None,
        false,
        None,
        None,
    );
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    assert!(rendered.saw_remote);
    assert!(text.contains("🖼 webpic"));
}

#[test]
fn links_are_collected_with_ranges() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);
    let rendered = preview::render(
        &buffer,
        "See [the site](https://x.org) and [a doc](files/doc.pdf).\n\n| [t](https://t.io) |\n| - |\n",
        None,
        false,
        None,
        None,
    );
    // Table links are inert (cell text is emitted out of order).
    assert_eq!(rendered.links.len(), 2);
    let text = |start: i32, end: i32| {
        buffer
            .text(&buffer.iter_at_offset(start), &buffer.iter_at_offset(end), false)
            .to_string()
    };
    assert_eq!(text(rendered.links[0].start, rendered.links[0].end), "the site");
    assert_eq!(rendered.links[0].dest, "https://x.org");
    assert_eq!(text(rendered.links[1].start, rendered.links[1].end), "a doc");
    assert_eq!(rendered.links[1].dest, "files/doc.pdf");
}

#[test]
fn task_checkboxes_point_at_their_source_brackets() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);
    // Non-ASCII before each marker: offsets must be chars, not bytes.
    let md = "# Tø\n\n- [ ] é one\n- [X] two\n\n1. [x] three\n";
    let rendered = preview::render(&buffer, md, None, false, None, None);
    let source: Vec<char> = md.chars().collect();
    let found: Vec<(String, String, bool)> = rendered
        .checkboxes
        .iter()
        .map(|c| {
            let glyph = buffer.iter_at_offset(c.start).char().to_string();
            let s = c.source as usize;
            (glyph, source[s..s + 3].iter().collect(), c.checked)
        })
        .collect();
    assert_eq!(
        found,
        [
            ("☐".into(), "[ ]".into(), false),
            ("☑".into(), "[X]".into(), true),
            ("☑".into(), "[x]".into(), true),
        ]
    );
}

#[test]
fn table_rows_map_to_their_source_lines() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);
    // Non-ASCII before the table: offsets must be chars, not bytes.
    let md = "Før\n\n| A | Bee |\n| - | --- |\n| 1 | 2 |\n| ø | 4 |\n";
    let rendered = preview::render(&buffer, md, None, false, None, None);
    let source: Vec<char> = md.chars().collect();
    let line_of = |off: i32| source[..off as usize].iter().filter(|&&c| c == '\n').count() as i32;
    let rows: Vec<(i32, String)> = rendered
        .table_rows
        .iter()
        .map(|r| {
            let text = buffer
                .text(&buffer.iter_at_offset(r.start), &buffer.iter_at_offset(r.end), false)
                .to_string();
            let source = r.source.expect("every rendered table row records its source");
            (line_of(source) + r.separator as i32, text.trim_end().to_string())
        })
        .collect();
    assert_eq!(
        rows,
        [
            (2, "A │ Bee".to_string()),
            (3, "──┼────".to_string()),
            (4, "1 │ 2".to_string()),
            (5, "ø │ 4".to_string()),
        ]
    );
    // Each recorded range is exactly one preview line.
    for r in &rendered.table_rows {
        assert!(buffer.iter_at_offset(r.start).starts_line());
        assert!(buffer.iter_at_offset(r.end).ends_line());
    }
}

#[test]
fn splice_produces_identical_content() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let live = gtk4::TextBuffer::new(None);
    preview::setup_tags(&live);

    let render = |buffer: &gtk4::TextBuffer, md: &str| {
        preview::render(buffer, md, None, false, None, None);
    };
    render(&live, "# One\n\nAlpha beta gamma\n\n- a\n- b\n\n**bold** end\n");

    // An edit in the middle, then one at the end, then a full change.
    for md in [
        "# One\n\nAlpha CHANGED beta gamma\n\n- a\n- b\n\n**bold** end\n",
        "# One\n\nAlpha CHANGED beta gamma\n\n- a\n- b\n\n**bold** end plus\n",
        "Totally different\n",
        "",
    ] {
        let scratch = gtk4::TextBuffer::new(Some(&live.tag_table()));
        render(&scratch, md);
        preview::splice(&live, &scratch);
        let live_text = live.slice(&live.start_iter(), &live.end_iter(), true).to_string();
        let scratch_text = scratch
            .slice(&scratch.start_iter(), &scratch.end_iter(), true)
            .to_string();
        assert_eq!(live_text, scratch_text, "after splicing {md:?}");
    }
}

/// Char offset of the first occurrence of `needle` in `haystack`.
fn find_char_offset(haystack: &[char], needle: &str) -> i32 {
    let needle: Vec<char> = needle.chars().collect();
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present") as i32
}

#[test]
fn sections_and_plain_render() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);

    let md = "# A\n\nbody a\n\n## A1\n\nbody a1\n\n# B\n\nbody b\n";
    let rendered = preview::render(&buffer, md, None, false, None, None);
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    assert!(text.starts_with("▾ A"), "headings get a disclosure marker");
    assert_eq!(rendered.sections.len(), 3);
    let (a, a1, b) = (
        &rendered.sections[0],
        &rendered.sections[1],
        &rendered.sections[2],
    );
    // A runs to B's marker; the nested A1 ends there too; B runs to the end.
    assert_eq!(a.end, b.marker_start);
    assert_eq!(a1.end, b.marker_start);
    assert_eq!(b.end, buffer.char_count());
    assert!(a.key != a1.key && a.key != b.key);
    let glyph = buffer
        .text(
            &buffer.iter_at_offset(a.marker_start),
            &buffer.iter_at_offset(a.marker_end),
            false,
        )
        .to_string();
    assert_eq!(glyph, "▾ ");
    assert!(a.content_start < a.end, "section body is inside the range");

    // Plain rendering: verbatim text with an exact 1:1 source map.
    let rendered = preview::render_plain(&buffer, "{ \"a\": 1 }\nline2", true);
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    assert_eq!(text, "{ \"a\": 1 }\nline2");
    assert!(rendered.sections.is_empty());
    assert_eq!(rendered.source_map.source_offset(3), Some(3));
    assert_eq!(rendered.source_map.preview_offset(12), Some(12));
}


/// Each character with the rendering tags applied to it. Tags that `ui.rs`
/// overlays after rendering (folding, find highlights) are skipped — the
/// splice is not responsible for reproducing those.
fn tagged(buffer: &gtk4::TextBuffer) -> Vec<(char, Vec<String>)> {
    let overlay = ["hidden", "find-match", "find-current"];
    let text = buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string();
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            let iter = buffer.iter_at_offset(i as i32);
            let mut names: Vec<String> = iter
                .tags()
                .iter()
                .filter_map(|t| t.name().map(|n| n.to_string()))
                .filter(|n| !overlay.contains(&n.as_str()))
                .collect();
            names.sort();
            (ch, names)
        })
        .collect()
}

/// Splice `after` onto a buffer holding `before` and compare the result
/// with what a full re-render produces — tags included.
fn assert_splice_matches_full_render(before: &str, after: &str) {
    let render = |b: &gtk4::TextBuffer, md: &str| preview::render(b, md, None, false, None, None);
    let live = gtk4::TextBuffer::new(None);
    preview::setup_tags(&live);
    render(&live, before);

    let scratch = gtk4::TextBuffer::new(Some(&live.tag_table()));
    render(&scratch, after);
    preview::splice(&live, &scratch);

    let reference = gtk4::TextBuffer::new(Some(&live.tag_table()));
    render(&reference, after);

    let (got, want) = (tagged(&live), tagged(&reference));
    if let Some((i, (g, w))) = got.iter().zip(&want).enumerate().find(|(_, (g, w))| g != w) {
        panic!(
            "{before:?} -> {after:?}\n  offset {i}: spliced {:?} with tags {:?}, \
             but a full render gives {:?} with tags {:?}",
            g.0, g.1, w.0, w.1
        );
    }
    assert_eq!(got.len(), want.len(), "{before:?} -> {after:?}: length differs");
}

#[test]
fn splice_updates_formatting_of_unchanged_text() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    // Each case leaves some visible text byte-for-byte identical while
    // changing its formatting — the shape a text-only diff cannot see,
    // because the unchanged text falls inside the common prefix or suffix
    // and never gets re-copied.
    let cases = [
        // The reported case: a heading missing its space, then corrected.
        (
            "## Test\nfadsga\n###Two\n#### Tree\n### Four\n",
            "## Test\nfadsga\n### Two\n#### Tree\n### Four\n",
        ),
        // ...and back, so the tags have to be removed as well as added.
        (
            "## Test\nfadsga\n### Two\n#### Tree\n### Four\n",
            "## Test\nfadsga\n###Two\n#### Tree\n### Four\n",
        ),
        // Same word, different heading level.
        ("### Same words here\n", "#### Same words here\n"),
        // Emphasis changing kind, and disappearing entirely.
        ("a **word** b\n", "a *word* b\n"),
        ("a *word* b\n", "a word b\n"),
        ("use `code` here\n", "use code here\n"),
        // A list marker appearing in front of otherwise identical text.
        ("-item one\n", "- item one\n"),
        // A heading gaining trailing content after it.
        ("# Head\n\nbody\n", "# Head\n\n**body**\n"),
    ];
    for (before, after) in cases {
        assert_splice_matches_full_render(before, after);
    }
}

#[test]
fn splice_ignores_the_folding_and_find_overlays() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let render = |b: &gtk4::TextBuffer, md: &str| preview::render(b, md, None, false, None, None);
    let md = "# One\n\nAlpha beta\n\n## Two\n\nGamma delta\n";
    let live = gtk4::TextBuffer::new(None);
    preview::setup_tags(&live);
    render(&live, md);

    // What ui.rs does after rendering: hide a collapsed section's body.
    let (start, end) = (live.iter_at_offset(6), live.iter_at_offset(16));
    live.apply_tag_by_name("hidden", &start, &end);

    // Re-render identical content. The overlay exists only on the live
    // buffer, so counting it in the diff would make everything from the
    // fold onwards look changed and rewrite it for nothing.
    let scratch = gtk4::TextBuffer::new(Some(&live.tag_table()));
    render(&scratch, md);
    preview::splice(&live, &scratch);

    // Surviving tags prove the region was left alone: a delete/reinsert
    // would have dropped them.
    let hidden = live.tag_table().lookup("hidden").expect("hidden tag");
    assert!(
        live.iter_at_offset(10).has_tag(&hidden),
        "an unchanged document must not be re-spliced just because it is folded"
    );
}

#[test]
fn images_in_table_cells_stay_in_their_cell() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);
    let pixbuf = gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, 16, 16)
        .expect("pixbuf");
    let png = pixbuf.save_to_bufferv("png", &[]).expect("png");
    let loader = move |dest: &str| (dest == "pic.png").then(|| png.clone());

    let markdown = "Intro\n\n| Pic | Name |\n| - | - |\n| ![cat](pic.png) | Tom |\n";
    preview::render(&buffer, markdown, None, false, None, Some(&loader));
    let text = buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string();

    // The thumbnail used to be inserted the moment the image was parsed —
    // above the table, since cell text is only emitted when the table ends.
    assert!(!text.contains('\u{FFFC}'), "no thumbnail may escape the table: {text:?}");
    assert!(text.contains("🖼 cat │ Tom"), "the cell keeps a placeholder and alt text: {text:?}");

    // Outside a table the same image still renders as a thumbnail.
    preview::render(&buffer, "![cat](pic.png)\n", None, false, None, Some(&loader));
    let text = buffer.slice(&buffer.start_iter(), &buffer.end_iter(), true).to_string();
    assert!(text.contains('\u{FFFC}'));
}

#[test]
fn wide_tables_wrap_inside_their_columns() {
    if gtk4::init().is_err() {
        eprintln!("no display available, skipping");
        return;
    }
    let buffer = gtk4::TextBuffer::new(None);
    preview::setup_tags(&buffer);
    let md = "| Key | Description |\n| --- | --- |\n| a | the quick brown fox jumps over the lazy dog |\n";
    let rendered = preview::render_fitted(&buffer, md, None, false, None, None, Some(20));
    let lines: Vec<String> = rendered
        .table_rows
        .iter()
        .map(|r| {
            buffer.text(&buffer.iter_at_offset(r.start), &buffer.iter_at_offset(r.end), false).to_string()
        })
        .collect();
    assert!(lines.len() > 3, "the long row wraps onto several lines: {lines:?}");
    for line in &lines {
        assert!(line.chars().count() <= 20, "{line:?} overflows");
        // Every line, continuation lines included, keeps the column border.
        assert_eq!(line.chars().position(|c| c == '│' || c == '┼'), Some(4), "{line:?}");
    }
    // All the wrapped lines of the body row point at the same source row.
    let body: Vec<_> = rendered.table_rows.iter().filter(|r| !r.separator).skip(1).collect();
    assert!(body.iter().all(|r| r.source == body[0].source));
    let joined: Vec<String> = lines[2..].iter().map(|l| l[l.find('│').unwrap() + "│ ".len()..].to_string()).collect();
    assert_eq!(joined.join(" "), "the quick brown fox jumps over the lazy dog");
}
