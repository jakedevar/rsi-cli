//! Bounded local-image ingestion for the Harness agent loop.
// Input and payload bounds are adapted from PrimeIntellect-ai/prime-agent
// `packages/coding-agent/skills/attach-image/src/attach_image/attach_image.py`
// @ cd1f215c (MIT); see THIRD_PARTY_NOTICES.md.

use super::{HarnessTool, ToolExecutionMode, ToolResult, is_system_blocked};
use crate::path_safety::resolve_sandboxed_path;
use crate::session::harness::types::ToolContentBlock;
use base64::Engine as _;
use image::{DynamicImage, ImageFormat};
use std::path::Path;

const MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
const MAX_PIXELS: u64 = 36_000_000;
const MAX_BASE64_CHARS: usize = 350_000;
const MAX_DIMENSION: u32 = 1200;
const JPEG_QUALITY_LADDER: [u8; 5] = [82, 72, 60, 48, 36];

pub struct ViewImageTool {
    image_input_supported: bool,
}

impl ViewImageTool {
    pub fn new(image_input_supported: bool) -> Self {
        Self {
            image_input_supported,
        }
    }

    fn failure(message: impl Into<String>) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error_msg: Some(message.into()),
        }
    }

    async fn execute_inner(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        if !self.image_input_supported {
            return Self::failure(
                "view_image unsupported: the selected model route does not accept image input",
            );
        }

        let Some(path) = args.get("path").and_then(|value| value.as_str()) else {
            return Self::failure("view_image requires a non-empty string path");
        };
        if path.is_empty() {
            return Self::failure("view_image requires a non-empty string path");
        }
        let detail = args
            .get("detail")
            .and_then(|value| value.as_str())
            .unwrap_or("high")
            .to_string();
        if !matches!(detail.as_str(), "auto" | "low" | "high") {
            return Self::failure(
                "view_image detail must be one of: auto, low, high (default: high)",
            );
        }

        let resolved = match resolve_sandboxed_path(working_dir, path) {
            Ok(resolved) => resolved,
            Err(error) => return Self::failure(format!("view_image path rejected: {error}")),
        };
        if is_system_blocked(&resolved) {
            return Self::failure("view_image path rejected: system path blocked");
        }

        let metadata = match tokio::fs::metadata(&resolved).await {
            Ok(metadata) => metadata,
            Err(error) => {
                return Self::failure(format!(
                    "view_image cannot stat '{}': {error}",
                    resolved.display()
                ));
            }
        };
        if !metadata.is_file() {
            return Self::failure(format!(
                "view_image path is not a regular file: {}",
                resolved.display()
            ));
        }
        if metadata.len() > MAX_INPUT_BYTES {
            return Self::failure(format!(
                "view_image file exceeds the 20 MiB input limit: {} bytes",
                metadata.len()
            ));
        }

        let bytes = match tokio::fs::read(&resolved).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Self::failure(format!(
                    "view_image cannot read '{}': {error}",
                    resolved.display()
                ));
            }
        };
        let reference = resolved.to_string_lossy().to_string();
        let encoded = match tokio::task::spawn_blocking(move || encode_bounded_image(bytes)).await {
            Ok(Ok(encoded)) => encoded,
            Ok(Err(error)) => return Self::failure(format!("view_image failed: {error}")),
            Err(_) => return Self::failure("view_image image processing was cancelled"),
        };

        let text = format!(
            "Viewed {reference} (source {}x{}, output {}x{}).",
            encoded.source_width, encoded.source_height, encoded.width, encoded.height
        );
        ToolResult::from_blocks(
            vec![
                ToolContentBlock::Text { text },
                ToolContentBlock::Image {
                    media_type: encoded.media_type.to_string(),
                    data: encoded.data,
                    detail: Some(detail),
                    reference: Some(reference),
                    width: Some(encoded.width),
                    height: Some(encoded.height),
                },
            ],
            false,
        )
    }
}

#[derive(Debug)]
struct EncodedImage {
    media_type: &'static str,
    data: String,
    width: u32,
    height: u32,
    source_width: u32,
    source_height: u32,
}

fn encode_bounded_image(bytes: Vec<u8>) -> Result<EncodedImage, String> {
    let format = image::guess_format(&bytes)
        .map_err(|error| format!("unsupported or corrupt image: {error}"))?;
    let media_type = media_type_for(format)?;
    let image = image::load_from_memory_with_format(&bytes, format)
        .map_err(|error| format!("cannot decode image: {error}"))?;

    let source_width = image.width();
    let source_height = image.height();
    if source_width == 0 || source_height == 0 {
        return Err("image has invalid dimensions".to_string());
    }
    if u64::from(source_width) * u64::from(source_height) > MAX_PIXELS {
        return Err(format!(
            "image exceeds the 36 megapixel limit: {source_width}x{source_height}"
        ));
    }

    let original = base64::engine::general_purpose::STANDARD.encode(&bytes);
    if source_width <= MAX_DIMENSION
        && source_height <= MAX_DIMENSION
        && original.len() <= MAX_BASE64_CHARS
    {
        return Ok(EncodedImage {
            media_type,
            data: original,
            width: source_width,
            height: source_height,
            source_width,
            source_height,
        });
    }

    let (width, height) = bounded_dimensions(source_width, source_height);
    let resized = resize_image(image, width, height);
    for quality in JPEG_QUALITY_LADDER {
        let encoded = encode_jpeg(&resized, quality)?;
        let data = base64::engine::general_purpose::STANDARD.encode(encoded);
        if data.len() <= MAX_BASE64_CHARS {
            return Ok(EncodedImage {
                media_type: "image/jpeg",
                data,
                width,
                height,
                source_width,
                source_height,
            });
        }
    }
    Err(format!(
        "image cannot fit the bounded 350,000-character payload after resize to {width}x{height}"
    ))
}

fn media_type_for(format: ImageFormat) -> Result<&'static str, String> {
    match format {
        ImageFormat::Png => Ok("image/png"),
        ImageFormat::Jpeg => Ok("image/jpeg"),
        ImageFormat::Gif => Ok("image/gif"),
        ImageFormat::WebP => Ok("image/webp"),
        format => Err(format!("unsupported image format: {format:?}")),
    }
}

fn bounded_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width <= MAX_DIMENSION && height <= MAX_DIMENSION {
        return (width, height);
    }
    let scale = f64::from(MAX_DIMENSION) / f64::from(width.max(height));
    let bounded_width = ((f64::from(width) * scale).round() as u32).max(1);
    let bounded_height = ((f64::from(height) * scale).round() as u32).max(1);
    (bounded_width, bounded_height)
}

fn resize_image(image: DynamicImage, width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(image::imageops::resize(
        &image.to_rgb8(),
        width,
        height,
        image::imageops::FilterType::Lanczos3,
    ))
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
        .encode_image(image)
        .map_err(|error| format!("cannot encode resized image: {error}"))?;
    Ok(encoded)
}

#[cfg(test)]
pub(crate) fn test_png_image_block(detail: Option<&str>) -> ToolContentBlock {
    let mut bytes = Vec::new();
    image::RgbImage::from_pixel(1, 1, image::Rgb([12, 34, 56]))
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .expect("encode test PNG");
    ToolContentBlock::Image {
        media_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        detail: detail.map(str::to_string),
        reference: Some("small.png".into()),
        width: Some(1),
        height: Some(1),
    }
}

#[async_trait::async_trait]
impl HarnessTool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "View a local PNG, JPEG, GIF, or WebP image. Returns a bounded image for vision-capable models."
    }

    fn parameters_json(&self) -> &str {
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Local image path, relative to the working directory."},"detail":{"type":"string","enum":["auto","low","high"],"default":"high","description":"Image detail hint."}},"required":["path"],"additionalProperties":false}"#
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::ParallelSafe
    }

    async fn execute(&self, args: serde_json::Value, working_dir: &Path) -> ToolResult {
        self.execute_inner(args, working_dir).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn write_image(dir: &Path, name: &str, image: RgbImage) -> PathBuf {
        let path = dir.join(name);
        image.save(&path).expect("write image");
        path
    }

    async fn execute(tool: &ViewImageTool, path: &Path, working_dir: &Path) -> ToolResult {
        tool.execute(
            json!({"path": path.to_string_lossy(), "detail": "high"}),
            working_dir,
        )
        .await
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn valid_png_returns_bounded_image_block_and_metadata() {
        let dir = crate::test_support::disk_backed_tempdir("view-image-");
        let path = write_image(
            dir.path(),
            "small.png",
            RgbImage::from_pixel(4, 3, Rgb([12, 34, 56])),
        );
        let result = execute(&ViewImageTool::new(true), &path, dir.path()).await;

        assert!(result.success, "{:?}", result.error_msg);
        assert!(result.has_typed_blocks());
        let event_message = crate::session::harness::types::ChatMessage::tool_result(
            "view_image",
            result.output.clone(),
        );
        let event_text = event_message.visible_tool_text();
        assert!(
            !event_text.contains("iVBOR"),
            "payload leaked into text event: {event_text}"
        );
        let metadata = result.event_metadata().expect("metadata");
        assert_eq!(metadata["images"][0]["media_type"], "image/png");
        assert_eq!(
            metadata["images"][0]["reference"].as_str().unwrap(),
            path.to_string_lossy().as_ref()
        );
        assert_eq!(metadata["images"][0]["width"], 4);
        assert_eq!(metadata["images"][0]["height"], 3);
        assert!(metadata["images"][0]["base64_chars"].as_u64().unwrap() <= 350_000);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn corrupt_nonimage_and_oversized_files_return_bounded_errors() {
        let dir = crate::test_support::disk_backed_tempdir("view-image-");
        let corrupt = dir.path().join("corrupt.png");
        fs::write(&corrupt, b"\x89PNG\r\n\x1a\nnot an image").expect("write");
        let nonimage = dir.path().join("nonimage.txt");
        fs::write(&nonimage, b"plain text").expect("write");
        let oversized = dir.path().join("oversized.png");
        fs::write(
            &oversized,
            vec![0; (MAX_INPUT_BYTES + 1).try_into().unwrap()],
        )
        .expect("write");

        for path in [&corrupt, &nonimage, &oversized] {
            let result = execute(&ViewImageTool::new(true), path, dir.path()).await;
            assert!(!result.success, "{path:?} unexpectedly succeeded");
            let message = result.error_msg.expect("error");
            assert!(message.starts_with("view_image"), "{message}");
            assert!(message.len() < 300, "{message}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn non_vision_route_and_escape_are_rejected_before_processing() {
        let dir = crate::test_support::disk_backed_tempdir("view-image-");
        let image = write_image(
            dir.path(),
            "small.png",
            RgbImage::from_pixel(2, 2, Rgb([1, 2, 3])),
        );
        let non_vision = execute(&ViewImageTool::new(false), &image, dir.path()).await;
        assert_eq!(
            non_vision.error_msg.as_deref(),
            Some("view_image unsupported: the selected model route does not accept image input")
        );

        let outside = crate::test_support::disk_backed_tempdir("view-image-outside-");
        let outside_image = write_image(
            outside.path(),
            "outside.png",
            RgbImage::from_pixel(2, 2, Rgb([1, 2, 3])),
        );
        let escape = execute(&ViewImageTool::new(true), &outside_image, dir.path()).await;
        assert!(!escape.success);
        let message = escape.error_msg.expect("error");
        assert!(message.contains("escapes working directory"), "{message}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn large_png_is_resized_and_limited_to_payload_cap() {
        let dir = crate::test_support::disk_backed_tempdir("view-image-");
        let mut image = RgbImage::new(1600, 1200);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([(x % 256) as u8, ((x / 3) % 256) as u8, 99]);
        }
        let path = write_image(dir.path(), "large.png", image);
        let result = execute(&ViewImageTool::new(true), &path, dir.path()).await;

        assert!(result.success, "{:?}", result.error_msg);
        let metadata = result.event_metadata().expect("metadata");
        assert_eq!(metadata["images"][0]["media_type"], "image/jpeg");
        assert_eq!(metadata["images"][0]["width"], 1200);
        assert_eq!(metadata["images"][0]["height"], 900);
        assert!(metadata["images"][0]["base64_chars"].as_u64().unwrap() <= 350_000);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[tokio::test]
    async fn image_over_the_pixel_limit_fails_before_resize() {
        let image = RgbImage::from_pixel(6001, 6000, Rgb([12, 34, 56]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode PNG");

        let error = encode_bounded_image(bytes).expect_err("pixel limit");
        assert!(error.contains("36 megapixel limit"), "{error}");
        assert!(error.contains("6001x6000"), "{error}");
    }
}
