// Dev tool: checks that incremental keyword highlighting agrees with a full
// re-tag, and times both. The tagging runs on the main loop on every debounced
// keystroke, so its cost is felt directly as typing latency.
//   cargo run --release --example hlbench
//
// This mirrors what App::apply_highlight does in ui.rs; keep the two in step.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use bindle::doc::DocKind;
use bindle::highlight;
use gtk4::prelude::*;

fn document(blocks: usize) -> String {
    let mut md = String::new();
    for i in 0..blocks {
        md.push_str(&format!("## Section {i}\n\n"));
        md.push_str("Some *italic* and **bold** text with `code` and [a link](http://x).\n\n");
        md.push_str("- first **item**\n- second `item`\n- third *item*\n\n");
        md.push_str("```\nfenced code\n```\n\n");
    }
    md
}

/// Every (start, end, tag) currently on the buffer, for comparing two runs.
fn tagging(buffer: &gtk4::TextBuffer) -> Vec<(i32, i32, String)> {
    let table = buffer.tag_table();
    let mut out = Vec::new();
    for name in highlight::TAGS {
        let Some(tag) = table.lookup(name) else { continue };
        let mut iter = buffer.start_iter();
        if !iter.starts_tag(Some(&tag)) && !iter.forward_to_tag_toggle(Some(&tag)) {
            continue;
        }
        loop {
            let start = iter.offset();
            if !iter.forward_to_tag_toggle(Some(&tag)) {
                break;
            }
            out.push((start, iter.offset(), name.to_string()));
            if !iter.forward_to_tag_toggle(Some(&tag)) {
                break;
            }
        }
    }
    out.sort();
    out
}

fn full_apply(buffer: &gtk4::TextBuffer, text: &str, kind: DocKind) {
    let (start, end) = (buffer.start_iter(), buffer.end_iter());
    for tag in highlight::TAGS {
        buffer.remove_tag_by_name(tag, &start, &end);
    }
    for span in highlight::spans(text, kind) {
        buffer.apply_tag_by_name(
            span.tag,
            &buffer.iter_at_offset(span.start),
            &buffer.iter_at_offset(span.end),
        );
    }
}

/// The incremental path: re-tag only the lines whose signature moved.
fn incremental(
    buffer: &gtk4::TextBuffer,
    text: &str,
    kind: DocKind,
    baseline: &Rc<RefCell<Vec<u64>>>,
) -> usize {
    let spans = highlight::spans(text, kind);
    let (starts, sigs) = highlight::line_sigs(text, &spans);
    let dirty = if baseline.borrow().len() != sigs.len() {
        Some((0, sigs.len() - 1))
    } else {
        highlight::dirty_lines(&baseline.borrow(), &sigs)
    };
    *baseline.borrow_mut() = sigs;
    let Some((lo, hi)) = dirty else { return 0 };
    let from = starts[lo];
    let to = starts.get(hi + 1).copied().unwrap_or_else(|| buffer.end_iter().offset());
    let (a, b) = (buffer.iter_at_offset(from), buffer.iter_at_offset(to));
    for tag in highlight::TAGS {
        buffer.remove_tag_by_name(tag, &a, &b);
    }
    for span in spans.iter().skip_while(|s| s.start < from).take_while(|s| s.start < to) {
        buffer.apply_tag_by_name(
            span.tag,
            &buffer.iter_at_offset(span.start),
            &buffer.iter_at_offset(span.end),
        );
    }
    hi - lo + 1
}

fn main() {
    if gtk4::init().is_err() {
        eprintln!("no display available");
        return;
    }
    let kind = DocKind::Markdown;
    let blocks = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(400);
    let text = document(blocks);
    let lines = text.lines().count();
    println!("document: {} chars, {lines} lines\n", text.chars().count());

    // Typing one character into a paragraph in the middle.
    let cut = text.char_indices().nth(text.chars().count() / 2).unwrap().0;
    let line_start = text[..cut].rfind('\n').map_or(0, |i| i + 1);
    let mut edited = text.clone();
    edited.insert(line_start, 'X');

    // Reference: what a full re-tag produces for the edited text.
    let reference = gtk4::TextBuffer::new(None);
    highlight::setup_tags(&reference);
    reference.set_text(&edited);
    let t = Instant::now();
    full_apply(&reference, &edited, kind);
    let full_ms = t.elapsed().as_secs_f64() * 1000.0;
    let want = tagging(&reference);

    // Incremental: tag the original, edit the buffer, tag again.
    let (live, baseline) = tracked_buffer(&text);
    incremental(&live, &text, kind, &baseline);
    let mut at = live.iter_at_offset(text[..line_start].chars().count() as i32);
    live.insert(&mut at, "X");
    let t = Instant::now();
    let dirty = incremental(&live, &edited, kind, &baseline);
    let inc_ms = t.elapsed().as_secs_f64() * 1000.0;
    let got = tagging(&live);

    // How much of the incremental cost is the scan, which stays O(document)
    // however few lines end up dirty — this is the floor, and what
    // HIGHLIGHT_MAX_BYTES in ui.rs exists to cap.
    let t = Instant::now();
    let s = highlight::spans(&edited, kind);
    let _ = highlight::line_sigs(&edited, &s);
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;

    println!("full re-tag:  {full_ms:7.2} ms   ({} spans)", want.len());
    println!("incremental:  {inc_ms:7.2} ms   ({dirty} dirty line(s))");
    println!("  of which scan: {scan_ms:6.2} ms   (tagging: {:.2} ms)", inc_ms - scan_ms);
    println!("speedup:      {:7.1}x\n", full_ms / inc_ms.max(0.0001));

    if got == want {
        println!("tagging matches the full re-tag exactly");
    } else {
        println!("MISMATCH: {} spans vs {} expected", got.len(), want.len());
        for (a, b) in got.iter().zip(&want).filter(|(a, b)| a != b).take(5) {
            println!("  got {a:?}\n  want {b:?}");
        }
        std::process::exit(1);
    }

    // A fence toggle has to restyle everything below it, incremental or not.
    // Edited in place, as typing does: set_text would drop every tag in the
    // buffer and invalidate the baseline (which is why load_text resets it).
    let heading = "## Section 1";
    let fenced = text.replacen(heading, "```", 1);
    let (live2, baseline2) = tracked_buffer(&text);
    incremental(&live2, &text, kind, &baseline2);
    let at = text.find(heading).expect("heading present");
    let from = text[..at].chars().count() as i32;
    let mut a = live2.iter_at_offset(from);
    let mut b = live2.iter_at_offset(from + heading.chars().count() as i32);
    live2.delete(&mut a, &mut b);
    let mut a = live2.iter_at_offset(from);
    live2.insert(&mut a, "```");
    let dirty = incremental(&live2, &fenced, kind, &baseline2);
    check("fence toggle", dirty, &live2, &fenced, kind);

    // A multi-line delete that pulls an untouched line under an opening
    // fence. By position its signature equals the deleted line it replaces,
    // so only a baseline that follows line identity notices it.
    let small = "a\n```\nx\n```\nx\n";
    let (live3, baseline3) = tracked_buffer(small);
    incremental(&live3, small, kind, &baseline3);
    let (mut a, mut b) = (live3.iter_at_line(1).unwrap(), live3.iter_at_line(3).unwrap());
    live3.delete(&mut a, &mut b);
    let joined = "a\n```\nx\n";
    let dirty = incremental(&live3, joined, kind, &baseline3);
    check("multi-line delete", dirty, &live3, joined, kind);
}

/// A buffer whose edits keep `baseline` aligned, as the editor's does.
fn tracked_buffer(text: &str) -> (gtk4::TextBuffer, Rc<RefCell<Vec<u64>>>) {
    let buffer = gtk4::TextBuffer::new(None);
    highlight::setup_tags(&buffer);
    buffer.set_text(text);
    let baseline = Rc::new(RefCell::new(Vec::new()));
    highlight::track_edits(&buffer, baseline.clone());
    (buffer, baseline)
}

/// Compare `live`'s tagging with a full re-tag of `text`; exit 1 on mismatch.
fn check(what: &str, dirty: usize, live: &gtk4::TextBuffer, text: &str, kind: DocKind) {
    let reference = gtk4::TextBuffer::new(None);
    highlight::setup_tags(&reference);
    reference.set_text(text);
    full_apply(&reference, text, kind);
    let ok = tagging(live) == tagging(&reference);
    println!("{what}: {dirty} dirty line(s), tagging {}", if ok { "matches" } else { "MISMATCHES" });
    if !ok {
        std::process::exit(1);
    }
}
