//! The About box's changelog ("What's New"), built from the AppStream
//! metainfo. That file's `<releases>` list is the one place release notes are
//! written — `bump.sh` adds an entry per version, and GNOME Software reads the
//! same list — so the About box can never drift from it.

const METAINFO: &str = include_str!("../data/no.oyzmo.Bindle.metainfo.xml");

/// Oldest release the changelog shows. Earlier entries predate it and were
/// never written as short one-liners.
pub const FIRST_VERSION: &str = "1.6.2";

#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// One entry per `<p>`/`<li>`, whitespace-collapsed and still
    /// XML-escaped exactly as in the file — it goes straight back into markup.
    pub notes: Vec<String>,
}

/// Release notes markup for `adw::AboutDialog` (which accepts p/ul/li/em).
pub fn about_markup() -> String {
    markup(&releases(METAINFO, FIRST_VERSION))
}

/// Releases in file order (newest first), down to and including `first`.
/// Malformed input just ends the scan with what was found.
pub fn releases(metainfo: &str, first: &str) -> Vec<Release> {
    let mut out = Vec::new();
    let mut rest = metainfo;
    while let Some(at) = rest.find("<release ") {
        rest = &rest[at..];
        let Some(tag_end) = rest.find('>') else { break };
        let tag = &rest[..tag_end];
        let Some(version) = attr(tag, "version") else { break };
        rest = &rest[tag_end + 1..];
        let mut notes = Vec::new();
        if !tag.ends_with('/') {
            let close = rest.find("</release>").unwrap_or(rest.len());
            notes = note_texts(&rest[..close]);
            rest = &rest[close..];
        }
        let done = version == first;
        out.push(Release { version, notes });
        if done {
            break;
        }
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = tag.find(&key)? + key.len();
    let len = tag[start..].find('"')?;
    Some(tag[start..start + len].to_string())
}

fn note_texts(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let next = [("<p>", "</p>"), ("<li>", "</li>")]
            .into_iter()
            .filter_map(|(open, close)| rest.find(open).map(|i| (i, open, close)))
            .min();
        let Some((i, open, close)) = next else { break };
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(close) else { break };
        let text = after[..j].split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            out.push(text);
        }
        rest = &after[j + close.len()..];
    }
    out
}

pub fn markup(releases: &[Release]) -> String {
    releases
        .iter()
        .filter(|r| !r.notes.is_empty())
        .map(|r| {
            let items: String = r.notes.iter().map(|n| format!("<li>{n}</li>")).collect();
            format!("<p><em>{}</em></p><ul>{items}</ul>", r.version)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<releases>
    <release version="2.0.0" date="2026-10-01">
      <description>
        <ul>
          <li>One &amp; two</li>
          <li>Three</li>
        </ul>
      </description>
    </release>
    <release version="1.9.1" date="2026-09-30">
      <description>
        <p>A fix that wraps
        over two lines.</p>
      </description>
    </release>
    <release version="1.9.0" date="2026-09-29"/>
    <release version="1.8.0" date="2026-09-28">
      <description><p>Too old to show.</p></description>
    </release>
  </releases>"#;

    #[test]
    fn reads_paragraphs_and_list_items_down_to_the_first_version() {
        let got = releases(SAMPLE, "1.9.0");
        assert_eq!(
            got,
            vec![
                Release { version: "2.0.0".into(), notes: vec!["One &amp; two".into(), "Three".into()] },
                Release { version: "1.9.1".into(), notes: vec!["A fix that wraps over two lines.".into()] },
                Release { version: "1.9.0".into(), notes: vec![] },
            ]
        );
    }

    #[test]
    fn markup_skips_releases_without_notes() {
        let got = markup(&releases(SAMPLE, "1.9.0"));
        assert_eq!(
            got,
            "<p><em>2.0.0</em></p><ul><li>One &amp; two</li><li>Three</li></ul>\
             <p><em>1.9.1</em></p><ul><li>A fix that wraps over two lines.</li></ul>"
        );
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for bad in ["<release ", "<release version=\"1", "<release version=\"1\"><p>x", "<p></li>"] {
            let _ = markup(&releases(bad, FIRST_VERSION));
        }
    }

    /// Catches a version bumped by hand: Cargo.toml and the metainfo must
    /// agree, and the new version must say what changed.
    #[test]
    fn the_current_version_heads_the_changelog_with_notes() {
        let all = releases(METAINFO, FIRST_VERSION);
        let head = all.first().expect("metainfo has releases");
        assert_eq!(head.version, env!("CARGO_PKG_VERSION"), "run ./bump.sh instead of editing versions");
        assert!(!head.notes.is_empty(), "release {} has no notes", head.version);
        assert_eq!(all.last().map(|r| r.version.as_str()), Some(FIRST_VERSION));
    }
}
