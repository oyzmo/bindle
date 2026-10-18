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

    let (text, info) = bundle::open_pack(&pack).unwrap();
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
    bundle::save_pack(&pack, &info, "# Changed\n", &staged).unwrap();
    let (text, info) = bundle::open_pack(&pack).unwrap();
    assert_eq!(text, "# Changed\n");
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
    let (text, name) = bundle::open_bundle(&bundle_dir).unwrap();
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
