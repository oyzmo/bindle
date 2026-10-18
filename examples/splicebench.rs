// Dev tool: times preview::splice on a large, heavily formatted document.
// The splice runs on the main loop on every debounced render, so its cost is
// felt directly as typing latency.
//   cargo run --release --example splicebench

use std::time::Instant;

use gtk4::prelude::*;
use bindle::preview;

fn document(blocks: usize, marker: &str) -> String {
    let mut md = String::new();
    for i in 0..blocks {
        md.push_str(&format!("## Section {i}\n\n"));
        md.push_str("Some *italic* and **bold** text with `code` in it.\n\n");
        md.push_str("- first **item**\n- second `item`\n- third *item*\n\n");
        md.push_str(&format!("### Sub {i} {marker}\n\nbody text here\n\n"));
    }
    md
}

fn main() {
    if gtk4::init().is_err() {
        eprintln!("no display available");
        return;
    }
    let blocks = 300;
    let before = document(blocks, "x");
    // One character different, near the end of every subsection heading.
    let after = document(blocks, "y");
    println!("document: {} chars, {blocks} blocks", before.chars().count());

    let live = gtk4::TextBuffer::new(None);
    preview::setup_tags(&live);
    preview::render(&live, &before, None, false, None, None);

    // Pre-render both scratch buffers so only the splice is timed.
    let a = gtk4::TextBuffer::new(Some(&live.tag_table()));
    preview::render(&a, &after, None, false, None, None);
    let b = gtk4::TextBuffer::new(Some(&live.tag_table()));
    preview::render(&b, &before, None, false, None, None);

    // Alternate between the two so each splice has real work to do.
    let rounds = 40;
    let start = Instant::now();
    for i in 0..rounds {
        preview::splice(&live, if i % 2 == 0 { &a } else { &b });
    }
    let elapsed = start.elapsed();
    println!(
        "splice: {:.2} ms/call over {rounds} calls",
        elapsed.as_secs_f64() * 1000.0 / rounds as f64
    );

    // The no-op case matters just as much: it is what every render costs
    // when the edit is elsewhere or the debounce fires without a change.
    let same = gtk4::TextBuffer::new(Some(&live.tag_table()));
    preview::render(&same, &before, None, false, None, None);
    preview::splice(&live, &same);
    let start = Instant::now();
    for _ in 0..rounds {
        preview::splice(&live, &same);
    }
    println!(
        "splice (no change): {:.2} ms/call",
        start.elapsed().as_secs_f64() * 1000.0 / rounds as f64
    );
}
