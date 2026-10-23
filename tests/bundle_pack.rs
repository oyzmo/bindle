// TextBundle/TextPack container logic — pure file/zip IO, no display needed.

use std::collections::HashMap;
use std::fs;

use bindle::bundle;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bindle-bundle-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn textpack_roundtrip() {
    let dir = scratch("pack");
    let pack = dir.join("Doc.textpack");
    assert!(bundle::is_pack(&pack));

    // One in-memory asset, one streamed from disk.
    let on_disk = dir.join("attachment.pdf");
    fs::write(&on_disk, b"pdf bytes").unwrap();
    let assets = vec![
        ("pic.png".to_string(), bundle::AssetSource::Bytes(vec![1u8, 2, 3])),
        ("attachment.pdf".to_string(), bundle::AssetSource::File(on_disk.clone())),
    ];
    bundle::export_pack(&pack, "# Hi\n\n![pic](assets/pic.png)\n", &assets).unwrap();

    let (text, info, _) = bundle::open_pack(&pack).unwrap();
    assert!(text.starts_with("# Hi"));
    assert_eq!(bundle::read_asset(&pack, &info, "assets/pic.png", bundle::COPY_MAX_BYTES), Some(vec![1, 2, 3]));
    assert_eq!(
        bundle::read_asset(&pack, &info, "assets/attachment.pdf", bundle::COPY_MAX_BYTES),
        Some(b"pdf bytes".to_vec())
    );
    assert_eq!(bundle::read_asset(&pack, &info, "assets/missing.png", bundle::COPY_MAX_BYTES), None);
    // The decode cap applies only when requested.
    assert_eq!(bundle::read_asset(&pack, &info, "assets/pic.png", 2), None);

    // Saving replaces the text, keeps old assets, adds staged ones.
    let mut staged = HashMap::new();
    staged.insert("assets/new.png".to_string(), bundle::AssetSource::Bytes(vec![9u8]));
    staged.insert("assets/staged.pdf".to_string(), bundle::AssetSource::File(on_disk));
    let mut saved = bundle::save_pack(&pack, &info, "# Changed\n", &staged).unwrap();
    let (text, info, _) = bundle::open_pack(&pack).unwrap();
    assert_eq!(text, "# Changed\n");
    // What a save reports is what a fresh open of the pack finds.
    let mut reopened = info.entries.clone();
    saved.sort();
    reopened.sort();
    assert_eq!(saved, reopened);
    assert_eq!(bundle::read_asset(&pack, &info, "assets/pic.png", bundle::COPY_MAX_BYTES), Some(vec![1, 2, 3]));
    assert_eq!(bundle::read_asset(&pack, &info, "assets/new.png", bundle::COPY_MAX_BYTES), Some(vec![9]));
    assert_eq!(
        bundle::read_asset(&pack, &info, "assets/staged.pdf", bundle::COPY_MAX_BYTES),
        Some(b"pdf bytes".to_vec())
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn textbundle_open_and_detect() {
    let dir = scratch("bundle");
    let bundle_dir = dir.join("Notes.textbundle");
    fs::create_dir_all(&bundle_dir).unwrap();
    fs::write(bundle_dir.join("info.json"), "{}").unwrap();
    fs::write(bundle_dir.join("text.md"), "# Bundle\n").unwrap();

    assert!(bundle::is_bundle(&bundle_dir));
    assert!(!bundle::is_bundle(&dir));
    let (text, name, _) = bundle::open_bundle(&bundle_dir).unwrap();
    assert_eq!(text, "# Bundle\n");
    assert_eq!(name, "text.md");

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn readonly_temp_extraction() {
    let path = bundle::extract_readonly_temp("test-pack", "assets/doc.pdf", b"v1").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"v1");
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    // Re-extraction overwrites the read-only previous copy.
    let path = bundle::extract_readonly_temp("test-pack", "assets/doc.pdf", b"v2").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"v2");
    assert!(fs::metadata(&path).unwrap().permissions().readonly());
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(&path, perms).unwrap();
    fs::remove_file(&path).ok();
}

#[test]
fn asset_dest_collection_and_rewrite() {
    let md = "![a](pics/a.png) ![b](<my pic.png>) ![web](https://x.org/y.png)\n\
              [report](files/report.pdf) [site](https://x.org) [top](#heading)\n\
              [mail](mailto:a@b.c)\n";
    let dests = bundle::local_asset_dests(md);
    assert_eq!(
        dests,
        vec![
            "pics/a.png".to_string(),
            "my pic.png".to_string(),
            "files/report.pdf".to_string(),
        ]
    );

    let renames = vec![
        ("pics/a.png".to_string(), "a.png".to_string()),
        ("my pic.png".to_string(), "my pic.png".to_string()),
        ("files/report.pdf".to_string(), "report.pdf".to_string()),
    ];
    let out = bundle::rewrite_asset_dests(md, &renames);
    assert!(out.contains("![a](assets/a.png)"));
    assert!(out.contains("![b](<assets/my pic.png>)"));
    assert!(out.contains("[report](assets/report.pdf)"));
    assert!(out.contains("https://x.org/y.png"));
    assert!(out.contains("(#heading)"));
}

#[test]
fn a_legacy_pack_with_a_duplicate_entry_opens_decoded_and_saves() {
    use std::io::Write;
    let dir = scratch("legacy");
    let pack = dir.join("Old.textpack");
    let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut zip = zip::ZipWriter::new(fs::File::create(&pack).unwrap());
    zip.start_file("text.md", stored).unwrap();
    zip.write_all(b"bl\xE5b\xE6r\r\n").unwrap();
    zip.start_file("assets/a.pnX", stored).unwrap();
    zip.write_all(b"first").unwrap();
    zip.start_file("assets/a.png", stored).unwrap();
    zip.write_all(b"second").unwrap();
    zip.finish().unwrap();
    // The writer refuses duplicate names, so make one by renaming in place.
    let mut bytes = fs::read(&pack).unwrap();
    let (from, to) = (b"assets/a.pnX", b"assets/a.png");
    let mut i = 0;
    while let Some(p) = bytes[i..].windows(from.len()).position(|w| w == from) {
        bytes[i + p..i + p + to.len()].copy_from_slice(to);
        i += p + 1;
    }
    fs::write(&pack, bytes).unwrap();

    let (text, info, legacy) = bundle::open_pack(&pack).unwrap();
    assert_eq!(text, "blåbær\n");
    assert_eq!(legacy, Some("Windows-1252"));
    // zip's reader already keeps one entry per name, so a save never meets
    // the duplicate (the writer would refuse it).
    assert_eq!(info.entries.iter().filter(|n| *n == "assets/a.png").count(), 1);
    let before = bundle::read_asset(&pack, &info, "assets/a.png", bundle::COPY_MAX_BYTES);
    assert!(before.is_some());
    bundle::save_pack(&pack, &info, "saved\n", &HashMap::new()).unwrap();
    let (text, info, _) = bundle::open_pack(&pack).unwrap();
    assert_eq!(text, "saved\n");
    assert_eq!(bundle::read_asset(&pack, &info, "assets/a.png", bundle::COPY_MAX_BYTES), before);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_bundle_prefers_text_md_and_skips_directories() {
    let dir = scratch("prefer").join("Doc.textbundle");
    fs::create_dir_all(dir.join("text.md")).unwrap();
    fs::write(dir.join("text.txt"), "txt").unwrap();
    let (text, name, _) = bundle::open_bundle(&dir).unwrap();
    assert_eq!((text.as_str(), name.as_str()), ("txt", "text.txt"));
    fs::remove_dir(dir.join("text.md")).unwrap();
    fs::write(dir.join("text.md"), "md").unwrap();
    assert_eq!(bundle::open_bundle(&dir).unwrap().1, "text.md");
    // A missing bundle is still NotFound, which ui.rs relies on.
    let err = bundle::open_bundle(&dir.with_file_name("Gone.textbundle")).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    fs::remove_dir_all(dir.parent().unwrap()).ok();
}
