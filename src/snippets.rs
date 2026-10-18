use gtk4::prelude::*;

/// How a snippet is inserted into the editor buffer.
pub enum Kind {
    /// Wrap the selection (or a placeholder) in prefix/suffix, e.g. **bold**.
    Wrap {
        prefix: &'static str,
        suffix: &'static str,
        placeholder: &'static str,
    },
    /// Prefix every selected line (or the current line), e.g. "- " or "# ".
    LinePrefix(&'static str),
    /// Insert a standalone block on its own lines, e.g. a table skeleton.
    Block(&'static str),
    /// Open a file picker and insert an image reference (handled in ui.rs).
    ImagePicker,
    /// Ask for an image URL and insert a web image reference (handled in ui.rs).
    WebImage,
    /// Open a file picker and insert a link to the file (handled in ui.rs).
    LinkFile,
    /// Open a file picker and make the file part of the document: copied or
    /// staged into its assets and linked (handled in ui.rs).
    AttachFile,
}

pub struct Snippet {
    pub label: &'static str,
    pub icon: &'static str,
    pub keywords: &'static str,
    pub kind: Kind,
}

pub fn all() -> &'static [Snippet] {
    use Kind::*;
    static SNIPPETS: &[Snippet] = &[
        Snippet {
            label: "Heading 1",
            icon: "format-justify-left-symbolic",
            keywords: "h1 title header",
            kind: LinePrefix("# "),
        },
        Snippet {
            label: "Heading 2",
            icon: "format-justify-left-symbolic",
            keywords: "h2 subtitle header",
            kind: LinePrefix("## "),
        },
        Snippet {
            label: "Heading 3",
            icon: "format-justify-left-symbolic",
            keywords: "h3 header",
            kind: LinePrefix("### "),
        },
        Snippet {
            label: "Bold",
            icon: "format-text-bold-symbolic",
            keywords: "strong emphasis",
            kind: Wrap { prefix: "**", suffix: "**", placeholder: "bold text" },
        },
        Snippet {
            label: "Italic",
            icon: "format-text-italic-symbolic",
            keywords: "emphasis cursive",
            kind: Wrap { prefix: "*", suffix: "*", placeholder: "italic text" },
        },
        Snippet {
            label: "Strikethrough",
            icon: "format-text-strikethrough-symbolic",
            keywords: "delete cross out",
            kind: Wrap { prefix: "~~", suffix: "~~", placeholder: "crossed out" },
        },
        Snippet {
            label: "Bullet list",
            icon: "view-list-bullet-symbolic",
            keywords: "unordered list dash",
            kind: LinePrefix("- "),
        },
        Snippet {
            label: "Numbered list",
            icon: "view-list-ordered-symbolic",
            keywords: "ordered list",
            kind: LinePrefix("1. "),
        },
        Snippet {
            label: "Task list",
            icon: "checkbox-checked-symbolic",
            keywords: "todo checkbox check",
            kind: LinePrefix("- [ ] "),
        },
        Snippet {
            label: "Quote",
            icon: "format-indent-more-symbolic",
            keywords: "blockquote cite",
            kind: LinePrefix("> "),
        },
        Snippet {
            label: "Link",
            icon: "insert-link-symbolic",
            keywords: "url href hyperlink",
            kind: Wrap { prefix: "[", suffix: "](https://)", placeholder: "link text" },
        },
        Snippet {
            label: "Image",
            icon: "insert-image-symbolic",
            keywords: "picture photo file",
            kind: ImagePicker,
        },
        Snippet {
            label: "Image (web)",
            icon: "web-browser-symbolic",
            keywords: "picture photo url internet link online",
            kind: WebImage,
        },
        Snippet {
            label: "Link file",
            icon: "insert-link-symbolic",
            keywords: "pdf document local path reference",
            kind: LinkFile,
        },
        Snippet {
            label: "Attach file",
            icon: "mail-attachment-symbolic",
            keywords: "embed include bundle pdf document asset",
            kind: AttachFile,
        },
        Snippet {
            label: "Inline code",
            icon: "utilities-terminal-symbolic",
            keywords: "monospace tt",
            kind: Wrap { prefix: "`", suffix: "`", placeholder: "code" },
        },
        Snippet {
            label: "Code block",
            icon: "utilities-terminal-symbolic",
            keywords: "fence pre program source",
            kind: Wrap { prefix: "```\n", suffix: "\n```", placeholder: "code" },
        },
        Snippet {
            label: "Table",
            icon: "view-grid-symbolic",
            keywords: "grid columns rows",
            kind: Block("| Column 1 | Column 2 |\n| -------- | -------- |\n| Cell     | Cell     |"),
        },
        Snippet {
            label: "Horizontal rule",
            icon: "view-more-horizontal-symbolic",
            keywords: "divider separator line",
            kind: Block("---"),
        },
    ];
    SNIPPETS
}

/// Build the markdown for a web image from user input: normalizes the URL
/// (adds https:// when no scheme is given) and derives the alt text from the
/// file name in the URL. Returns (markdown, alt) or None for empty input.
pub fn web_image_markdown(input: &str) -> Option<(String, String)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let url = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let alt = url
        .split(['?', '#'])
        .next()
        .and_then(|path| path.rsplit('/').next())
        .map(|name| name.rsplit_once('.').map_or(name, |(stem, _)| stem))
        .map(|stem| stem.replace("%20", " ").replace(['-', '_'], " "))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "image".to_string());
    Some((format!("![{alt}]({url})"), alt))
}

/// True if the snippet matches a search query (case-insensitive).
pub fn matches(snippet: &Snippet, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || snippet.label.to_lowercase().contains(&q)
        || snippet.keywords.contains(&q)
}

pub fn apply(buffer: &gtk4::TextBuffer, snippet: &Snippet) {
    buffer.begin_user_action();
    match &snippet.kind {
        Kind::Wrap { prefix, suffix, placeholder } => {
            if let Some((mut start, mut end)) = buffer.selection_bounds() {
                let selected = buffer.text(&start, &end, false).to_string();
                buffer.delete(&mut start, &mut end);
                let offset = start.offset();
                buffer.insert(&mut start, &format!("{prefix}{selected}{suffix}"));
                let cursor = buffer.iter_at_offset(
                    offset
                        + prefix.chars().count() as i32
                        + selected.chars().count() as i32
                        + suffix.chars().count() as i32,
                );
                buffer.place_cursor(&cursor);
            } else {
                let mut iter = buffer.iter_at_mark(&buffer.get_insert());
                let offset = iter.offset();
                buffer.insert(&mut iter, &format!("{prefix}{placeholder}{suffix}"));
                // Select the placeholder so typing replaces it immediately.
                let sel_start = buffer.iter_at_offset(offset + prefix.chars().count() as i32);
                let sel_end = buffer.iter_at_offset(
                    offset + (prefix.chars().count() + placeholder.chars().count()) as i32,
                );
                buffer.select_range(&sel_start, &sel_end);
            }
        }
        Kind::LinePrefix(prefix) => {
            let (first, last) = match buffer.selection_bounds() {
                // A selection ending at the very start of a line (how
                // line-wise selections end) doesn't include that line.
                Some((start, end)) if end.starts_line() && end.line() > start.line() => {
                    (start.line(), end.line() - 1)
                }
                Some((start, end)) => (start.line(), end.line()),
                None => {
                    let line = buffer.iter_at_mark(&buffer.get_insert()).line();
                    (line, line)
                }
            };
            for line in first..=last {
                if let Some(mut iter) = buffer.iter_at_line(line) {
                    buffer.insert(&mut iter, prefix);
                }
            }
        }
        Kind::Block(text) => {
            let mut iter = buffer.iter_at_mark(&buffer.get_insert());
            let mut inserted = String::new();
            if !iter.starts_line() {
                inserted.push('\n');
            }
            inserted.push_str(text);
            inserted.push('\n');
            buffer.insert(&mut iter, &inserted);
            // Leave the cursor at the end of the block's last line (before the
            // trailing newline) so Enter can continue e.g. a table directly.
            let mut cursor = iter;
            cursor.backward_char();
            buffer.place_cursor(&cursor);
        }
        // These need a parent window (dialogs); ui.rs intercepts these kinds
        // before calling apply().
        Kind::ImagePicker | Kind::WebImage | Kind::LinkFile | Kind::AttachFile => {}
    }
    buffer.end_user_action();
}
