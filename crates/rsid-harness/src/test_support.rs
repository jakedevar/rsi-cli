//! Test fixtures shared by the moved tool tests.

use tempfile::TempDir;

/// A fresh temporary directory under `$CARGO_TARGET_DIR` (or the workspace
/// `target/`), canonicalized, never under `$TMPDIR`. `label` becomes the
/// directory-name prefix. Mirrors the daemon's fixture of the same name.
pub(crate) fn disk_backed_tempdir(label: &str) -> TempDir {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(rsi_common::manifest_dir!()).join("../../target"))
        .join("rsid-test-fixtures");
    std::fs::create_dir_all(&base).expect("create disk-backed test fixture root");
    let base = std::fs::canonicalize(&base).expect("canonicalize disk-backed test fixture root");
    tempfile::Builder::new()
        .prefix(label)
        .tempdir_in(base)
        .expect("create disk-backed test fixture")
}

/// A one-pixel PNG tool-result image, as the daemon's provider tests build it.
pub(crate) fn test_png_image_block(detail: Option<&str>) -> crate::types::ToolContentBlock {
    let mut bytes = Vec::new();
    image::RgbImage::from_pixel(1, 1, image::Rgb([12, 34, 56]))
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .expect("encode test PNG");
    crate::types::ToolContentBlock::Image {
        media_type: "image/png".into(),
        data: {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        },
        detail: detail.map(str::to_string),
        reference: Some("small.png".into()),
        width: Some(1),
        height: Some(1),
    }
}
