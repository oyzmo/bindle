//! Dev tool for the ODT import/export, meant to be paired with LibreOffice
//! (`soffice --headless --convert-to …`) to check both directions against
//! the real thing:
//!
//!   cargo run --example odt -- export in.md out.odt   # like "Export as ODT…"
//!   cargo run --example odt -- import in.odt          # prints the markdown
//!
//! Export resolves local images against the input file's directory.

use std::path::Path;

use bindle::export::Content;
use bindle::odf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["export", input, output] => {
            let text = std::fs::read_to_string(input).expect("read the markdown");
            let base = Path::new(input).parent().unwrap_or(Path::new(".")).to_path_buf();
            let resolve = move |dest: &str| std::fs::read(base.join(dest)).ok();
            let title = Path::new(output).file_stem().map(|s| s.to_string_lossy().to_string());
            odf::write_odt(
                Path::new(output),
                &title.unwrap_or_default(),
                &text,
                Content::Markdown { resolve: &resolve },
            )
            .expect("export");
        }
        ["import", input] => print!("{}", odf::import(Path::new(input)).expect("import")),
        _ => {
            eprintln!("usage: odt export in.md out.odt | odt import in.odt");
            std::process::exit(2);
        }
    }
}
