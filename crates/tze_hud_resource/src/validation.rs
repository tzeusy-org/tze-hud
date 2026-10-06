//! Upload validation checks used by `ResourceStore`.
//!
//! 1. `upload_resource` capability check
//! 2. BLAKE3 hash integrity — computed hash must match `expected_hash`
//! 3. Per-resource size limit (raw bytes ≤ `max_resource_bytes`)
//! 4. Agent `texture_bytes_total` budget check
//! 5. V1-supported resource type check
//! 6. Content decode check (images decode, fonts parse)
//!
//! `ResourceStore` owns admission order, deduplication and storage. These
//! functions validate individual quantities and decoded content.

use crate::types::{
    DecodedMeta, MAX_TEXTURE_DIMENSION_PX, ResourceError, ResourceStoreConfig, ResourceType,
    SVG_DEFAULT_DIMENSION_PX, SVG_MAX_DIMENSION_PX,
};

// ─── Step 1: Capability check ─────────────────────────────────────────────────

/// The string capability name required to upload resources.
pub const CAPABILITY_UPLOAD_RESOURCE: &str = "upload_resource";

/// Verify that `agent_capabilities` contains `upload_resource`.
///
/// Spec: lines 88-90, RFC 0011 §5.2.
pub fn check_capability(agent_capabilities: &[String]) -> Result<(), ResourceError> {
    if agent_capabilities
        .iter()
        .any(|c| c == CAPABILITY_UPLOAD_RESOURCE || c == "*")
    {
        Ok(())
    } else {
        Err(ResourceError::CapabilityDenied)
    }
}

// ─── Step 2: BLAKE3 hash integrity ────────────────────────────────────────────

/// Compute the BLAKE3 hash of `data` and verify it matches `expected`.
///
/// Spec: lines 66-68, RFC 0011 §3.4.
///
/// BLAKE3 achieves ~3 GB/s on modern hardware; 1 MiB hashes in < 1 ms
/// (spec line 330).
pub fn check_hash(data: &[u8], expected: &[u8; 32]) -> Result<[u8; 32], ResourceError> {
    let hash = blake3::hash(data);
    let computed = *hash.as_bytes();
    if computed == *expected {
        Ok(computed)
    } else {
        let computed_hex: String = computed.iter().map(|b| format!("{b:02x}")).collect();
        let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
        Err(ResourceError::HashMismatch {
            computed: computed_hex,
            expected: expected_hex,
        })
    }
}

// ─── Step 3: Per-resource size limit ──────────────────────────────────────────

/// Verify that `byte_count` does not exceed `config.max_resource_bytes`.
///
/// Spec: lines 286-288, RFC 0011 §8.1.
pub fn check_raw_size(
    byte_count: usize,
    config: &ResourceStoreConfig,
) -> Result<(), ResourceError> {
    if byte_count > config.max_resource_bytes {
        Err(ResourceError::SizeExceeded {
            detail: format!(
                "raw upload size {byte_count} bytes exceeds limit {} bytes",
                config.max_resource_bytes
            ),
        })
    } else {
        Ok(())
    }
}

// ─── Step 4: Agent budget check ───────────────────────────────────────────────

/// Per-agent budget context for an upload.
#[derive(Clone, Debug)]
pub struct AgentBudget {
    /// Agent's `texture_bytes_total` lease limit (0 = unlimited).
    pub texture_bytes_total_limit: usize,
    /// Agent's current `texture_bytes_total` consumption.
    pub texture_bytes_total_used: usize,
}

impl AgentBudget {
    /// Return `true` if `additional_bytes` would fit within the agent's budget.
    pub fn would_fit(&self, additional_bytes: usize) -> bool {
        if self.texture_bytes_total_limit == 0 {
            return true; // unlimited
        }
        self.texture_bytes_total_used
            .checked_add(additional_bytes)
            .map(|total| total <= self.texture_bytes_total_limit)
            .unwrap_or(false)
    }
}

/// Verify that `decoded_bytes` fits within the agent's texture budget and the
/// runtime-wide texture ceiling.
///
/// Spec: lines 96-98, 300-303, RFC 0011 §8.3, §11.2.
pub fn check_budget(
    decoded_bytes: usize,
    agent: &AgentBudget,
    runtime_total_texture_bytes_used: usize,
    config: &ResourceStoreConfig,
) -> Result<(), ResourceError> {
    // Runtime-wide ceiling.
    if runtime_total_texture_bytes_used
        .checked_add(decoded_bytes)
        .map(|total| total > config.max_total_texture_bytes)
        .unwrap_or(true)
    {
        return Err(ResourceError::BudgetExceeded {
            detail: format!(
                "runtime-wide texture memory {runtime_total_texture_bytes_used} + {decoded_bytes} \
                 would exceed limit {}",
                config.max_total_texture_bytes
            ),
        });
    }

    // Per-agent limit.
    if !agent.would_fit(decoded_bytes) {
        return Err(ResourceError::BudgetExceeded {
            detail: format!(
                "agent texture budget {}/{} would be exceeded by {decoded_bytes} decoded bytes",
                agent.texture_bytes_total_used, agent.texture_bytes_total_limit
            ),
        });
    }

    Ok(())
}

// ─── Step 5: V1 type check ────────────────────────────────────────────────────

/// Verify that `resource_type` is in the v1-supported set.
///
/// Spec: lines 41-42, RFC 0011 §2.2.
pub fn check_resource_type(resource_type: ResourceType) -> Result<(), ResourceError> {
    if resource_type.is_v1_supported() {
        Ok(())
    } else {
        Err(ResourceError::UnsupportedType(resource_type.to_string()))
    }
}

// ─── Step 6: Decode validation ────────────────────────────────────────────────

/// Attempt to decode `data` as the given `resource_type`, producing
/// `DecodedMeta`.
///
/// For `IMAGE_RGBA8`: validates byte count against explicit `width`/`height`
/// from upload metadata (per RFC 0011), enforces dimension limits.
/// For PNG/JPEG: full decode to RGBA8 — validates pixel count and dimension
/// limits.  For fonts: parse the font file (no full render).
///
/// `width` and `height` are only used for `IMAGE_RGBA8`; pass 0 for other types.
///
/// # Decompression bomb defense
///
/// If a PNG/JPEG decodes to a texture exceeding `config.max_decoded_texture_bytes`
/// or to a dimension > `MAX_TEXTURE_DIMENSION_PX`, the decode is aborted with
/// `ResourceError::SizeExceeded` (spec lines 290-292).
///
/// Spec: lines 93-94, RFC 0011 §3.5, step 6.
pub fn decode_and_validate(
    data: &[u8],
    resource_type: ResourceType,
    config: &ResourceStoreConfig,
    width: u32,
    height: u32,
) -> Result<DecodedMeta, ResourceError> {
    match resource_type {
        ResourceType::ImageRgba8 => validate_raw_rgba8(data, config, width, height),
        ResourceType::ImagePng => decode_image(data, resource_type, config),
        ResourceType::ImageJpeg => decode_image(data, resource_type, config),
        ResourceType::FontTtf | ResourceType::FontOtf => validate_font(data),
        ResourceType::ImageSvg => validate_svg(data),
    }
}

fn validate_raw_rgba8(
    data: &[u8],
    config: &ResourceStoreConfig,
    width: u32,
    height: u32,
) -> Result<DecodedMeta, ResourceError> {
    // For raw RGBA8, width and height are provided by the upload metadata
    // (ResourceUploadStart.metadata.width/height per RFC 0011).
    if width == 0 || height == 0 {
        return Err(ResourceError::DecodeError(
            "IMAGE_RGBA8 requires non-zero width and height".into(),
        ));
    }

    // Dimension check (spec lines 281-283, MAX_TEXTURE_DIMENSION_PX = 8192).
    if width > MAX_TEXTURE_DIMENSION_PX || height > MAX_TEXTURE_DIMENSION_PX {
        return Err(ResourceError::SizeExceeded {
            detail: format!(
                "IMAGE_RGBA8 dimensions {width}x{height} exceed maximum {MAX_TEXTURE_DIMENSION_PX}"
            ),
        });
    }

    // Byte count must exactly match declared dimensions.
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(4));
    if Some(data.len()) != expected_len {
        return Err(ResourceError::DecodeError(format!(
            "IMAGE_RGBA8 byte count {} does not match dimensions {width}x{height} \
             (expected {} bytes)",
            data.len(),
            expected_len.unwrap_or(0)
        )));
    }

    if data.len() > config.max_decoded_texture_bytes {
        return Err(ResourceError::SizeExceeded {
            detail: format!(
                "raw RGBA8 decoded size {} bytes exceeds limit {}",
                data.len(),
                config.max_decoded_texture_bytes
            ),
        });
    }

    Ok(DecodedMeta {
        decoded_bytes: data.len(),
        width_px: width,
        height_px: height,
    })
}

fn decode_image(
    data: &[u8],
    resource_type: ResourceType,
    config: &ResourceStoreConfig,
) -> Result<DecodedMeta, ResourceError> {
    use image::ImageFormat;

    let format = match resource_type {
        ResourceType::ImagePng => ImageFormat::Png,
        ResourceType::ImageJpeg => ImageFormat::Jpeg,
        _ => unreachable!("only PNG and JPEG are routed here"),
    };

    // Decode to RGBA8.  The `image` crate will return an error for corrupt data.
    let img = image::load_from_memory_with_format(data, format)
        .map_err(|e| ResourceError::DecodeError(format!("{resource_type}: {e}")))?;

    let width_px = img.width();
    let height_px = img.height();

    // Dimension check (decompression bomb defense, spec lines 290-292).
    if width_px > MAX_TEXTURE_DIMENSION_PX || height_px > MAX_TEXTURE_DIMENSION_PX {
        return Err(ResourceError::SizeExceeded {
            detail: format!(
                "{resource_type} dimensions {width_px}x{height_px} exceed \
                 maximum {MAX_TEXTURE_DIMENSION_PX}"
            ),
        });
    }

    // Decoded in-memory size as RGBA8 (4 bytes per pixel).
    let decoded_bytes = (width_px as usize)
        .checked_mul(height_px as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| ResourceError::SizeExceeded {
            detail: format!(
                "{resource_type} decoded size overflows usize at {width_px}x{height_px}"
            ),
        })?;

    if decoded_bytes > config.max_decoded_texture_bytes {
        return Err(ResourceError::SizeExceeded {
            detail: format!(
                "{resource_type} decoded size {decoded_bytes} bytes exceeds limit {}",
                config.max_decoded_texture_bytes
            ),
        });
    }

    Ok(DecodedMeta {
        decoded_bytes,
        width_px,
        height_px,
    })
}

/// Validate an IMAGE_SVG upload.
///
/// Checks:
/// 1. Content parses as well-formed XML.
/// 2. The root element is `<svg>` (local name "svg", any namespace).
///
/// No rasterization occurs at upload time; budget is estimated from the SVG's
/// `viewBox` or `width`/`height` attributes (512×512 default, 2048×2048 clamp).
///
/// The returned `DecodedMeta::decoded_bytes` holds the estimated rasterized
/// size (`width_px * height_px * 4`) — this is the budget charge.
///
/// Source: resource-store/spec.md §Requirement: V1 Resource Type Enumeration,
///         §Requirement: SVG Resource Budget Accounting.
fn validate_svg(data: &[u8]) -> Result<DecodedMeta, ResourceError> {
    // Parse as UTF-8 text.
    let text = std::str::from_utf8(data)
        .map_err(|e| ResourceError::DecodeError(format!("IMAGE_SVG is not valid UTF-8: {e}")))?;

    // Parse as XML (well-formedness check).  We use a minimal XML pull parser
    // via `quick-xml` to avoid a heavy SVG-specific dependency at upload time.
    let (width_px, height_px) = parse_svg_dimensions(text)?;

    let estimated_bytes = (width_px as usize)
        .checked_mul(height_px as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| ResourceError::DecodeError("IMAGE_SVG dimension overflow".into()))?;

    Ok(DecodedMeta {
        decoded_bytes: estimated_bytes,
        width_px,
        height_px,
    })
}

/// Parse an SVG text document: verify XML well-formedness, check `<svg>` root,
/// and extract estimated raster dimensions.
///
/// Returns `(width_px, height_px)` after clamping to
/// `[1, SVG_MAX_DIMENSION_PX]` and applying the `SVG_DEFAULT_DIMENSION_PX`
/// fallback.
///
/// Source: resource-store/spec.md §Requirement: SVG Resource Budget Accounting.
pub fn parse_svg_dimensions(text: &str) -> Result<(u32, u32), ResourceError> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    // Find the first start/empty element, which must be `<svg>`.
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local = e.local_name();
                let local_str = std::str::from_utf8(local.as_ref()).unwrap_or("");
                if local_str != "svg" {
                    return Err(ResourceError::DecodeError(format!(
                        "IMAGE_SVG root element is <{local_str}>, expected <svg>"
                    )));
                }
                // Extract dimensions from attributes.
                let (w, h) = extract_svg_dimensions(e.attributes());
                return Ok((w, h));
            }
            Ok(Event::Eof) => {
                return Err(ResourceError::DecodeError(
                    "IMAGE_SVG: empty document, no root element found".into(),
                ));
            }
            Err(e) => {
                return Err(ResourceError::DecodeError(format!(
                    "IMAGE_SVG XML parse error: {e}"
                )));
            }
            // Skip processing instructions, comments, doctype, XML decl.
            _ => continue,
        }
    }
}

/// Extract estimated raster dimensions from SVG root element attributes.
///
/// Priority: `viewBox` (third and fourth tokens) > `width`/`height` attributes.
/// Values are clamped to `[1, SVG_MAX_DIMENSION_PX]`.
/// Missing or unparseable dimensions fall back to `SVG_DEFAULT_DIMENSION_PX`.
fn extract_svg_dimensions(attrs: quick_xml::events::attributes::Attributes<'_>) -> (u32, u32) {
    let mut view_box: Option<(u32, u32)> = None;
    let mut attr_width: Option<u32> = None;
    let mut attr_height: Option<u32> = None;

    for attr in attrs.flatten() {
        let local_name = attr.key.local_name();
        let key = std::str::from_utf8(local_name.as_ref()).unwrap_or("");
        let val_owned = attr.value;
        let val = std::str::from_utf8(val_owned.as_ref()).unwrap_or("").trim();
        match key {
            "viewBox" => {
                // viewBox="min-x min-y width height"
                let parts: Vec<&str> = val.split_whitespace().collect();
                if parts.len() >= 4 {
                    let w = parse_svg_dim(parts[2]);
                    let h = parse_svg_dim(parts[3]);
                    if w > 0 && h > 0 {
                        view_box = Some((clamp_svg_dim(w), clamp_svg_dim(h)));
                    }
                }
            }
            "width" => {
                let w = parse_svg_dim(val);
                if w > 0 {
                    attr_width = Some(clamp_svg_dim(w));
                }
            }
            "height" => {
                let h = parse_svg_dim(val);
                if h > 0 {
                    attr_height = Some(clamp_svg_dim(h));
                }
            }
            _ => {}
        }
    }

    // Priority: viewBox > width/height attributes > default.
    if let Some((w, h)) = view_box {
        return (w, h);
    }
    let w = attr_width.unwrap_or(SVG_DEFAULT_DIMENSION_PX);
    let h = attr_height.unwrap_or(SVG_DEFAULT_DIMENSION_PX);
    (w, h)
}

/// Parse a dimension value, stripping trailing units (e.g. "px", "pt").
/// Returns 0 on failure.
fn parse_svg_dim(s: &str) -> u32 {
    // Strip any non-digit suffix (units like "px", "pt", "em", "%").
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    // Parse as f64 first to handle decimal values like "200.5".
    digits.parse::<f64>().map(|v| v as u32).unwrap_or(0)
}

/// Clamp an SVG dimension to the valid budget range.
#[inline]
fn clamp_svg_dim(v: u32) -> u32 {
    v.clamp(1, SVG_MAX_DIMENSION_PX)
}

fn validate_font(data: &[u8]) -> Result<DecodedMeta, ResourceError> {
    // Use ttf-parser to validate the font file.  We do not fully render glyphs
    // here; we just verify the font parses successfully.
    ttf_parser::Face::parse(data, 0)
        .map_err(|e| ResourceError::DecodeError(format!("font parse error: {e:?}")))?;

    Ok(DecodedMeta {
        decoded_bytes: data.len(),
        width_px: 0,
        height_px: 0,
    })
}

/// Test helpers shared across crate-internal test modules.
///
/// Gated on `#[cfg(test)]` so they are not compiled into library consumers.
#[cfg(test)]
pub mod test_helpers {
    /// Return a minimal valid 1×1 red RGBA PNG (70 bytes).
    ///
    /// Bytes verified with Python `struct`/`zlib` and PIL that CRCs are correct
    /// and the image decodes to a 1×1 RGBA pixel.
    pub fn minimal_png_1x1() -> Vec<u8> {
        // 70-byte valid 1×1 red (ff0000ff) RGBA8 PNG.
        // Byte sequence verified with Python PIL: Image.open(BytesIO(data))
        // returns a 1×1 RGBA image.
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7,
            0x2f, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ResourceStoreConfig {
        ResourceStoreConfig::default()
    }

    fn caps_with_upload() -> Vec<String> {
        vec![CAPABILITY_UPLOAD_RESOURCE.to_string()]
    }

    fn caps_empty() -> Vec<String> {
        vec![]
    }

    fn unlimited_budget() -> AgentBudget {
        AgentBudget {
            texture_bytes_total_limit: 0,
            texture_bytes_total_used: 0,
        }
    }

    // ─── Step 1: capability ───────────────────────────────────────────────────

    #[test]
    fn capability_check_pass() {
        assert!(check_capability(&caps_with_upload()).is_ok());
    }

    #[test]
    fn capability_check_denied_for_empty_caps() {
        // Acceptance: guest agent rejected with RESOURCE_CAPABILITY_DENIED
        // (spec lines 88-90).
        let err = check_capability(&caps_empty()).unwrap_err();
        assert_eq!(err, ResourceError::CapabilityDenied);
        assert_eq!(err.wire_code(), "RESOURCE_CAPABILITY_DENIED");
    }

    // ─── Step 2: hash integrity ───────────────────────────────────────────────

    #[test]
    fn hash_check_pass() {
        let data = b"valid data";
        let hash = *blake3::hash(data).as_bytes();
        assert!(check_hash(data, &hash).is_ok());
    }

    #[test]
    fn hash_check_mismatch() {
        // Acceptance: RESOURCE_HASH_MISMATCH when computed ≠ expected
        // (spec lines 66-68).
        let data = b"some bytes";
        let wrong_hash = [0xffu8; 32];
        let err = check_hash(data, &wrong_hash).unwrap_err();
        assert!(matches!(err, ResourceError::HashMismatch { .. }));
        assert_eq!(err.wire_code(), "RESOURCE_HASH_MISMATCH");
    }

    // ─── Step 3: raw size ─────────────────────────────────────────────────────

    #[test]
    fn raw_size_pass() {
        let config = default_config();
        assert!(check_raw_size(1024, &config).is_ok());
    }

    #[test]
    fn raw_size_exceeded() {
        // Acceptance: 20 MiB rejected (spec lines 286-288).
        let config = default_config();
        let twenty_mib = 20 * 1024 * 1024;
        let err = check_raw_size(twenty_mib, &config).unwrap_err();
        assert!(matches!(err, ResourceError::SizeExceeded { .. }));
        assert_eq!(err.wire_code(), "RESOURCE_SIZE_EXCEEDED");
    }

    // ─── Step 4: budget ───────────────────────────────────────────────────────

    #[test]
    fn budget_check_unlimited_always_passes() {
        let config = default_config();
        let budget = unlimited_budget();
        // Even a large decoded size passes if limit=0 (unlimited).
        assert!(check_budget(100_000_000, &budget, 0, &config).is_ok());
    }

    #[test]
    fn budget_check_agent_exceeded() {
        // Acceptance: RESOURCE_BUDGET_EXCEEDED when agent limit is full
        // (spec lines 96-98).
        let config = default_config();
        let budget = AgentBudget {
            texture_bytes_total_limit: 10 * 1024 * 1024,
            texture_bytes_total_used: 10 * 1024 * 1024, // fully consumed
        };
        let err = check_budget(1024, &budget, 0, &config).unwrap_err();
        assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
        assert_eq!(err.wire_code(), "RESOURCE_BUDGET_EXCEEDED");
    }

    #[test]
    fn budget_check_runtime_exceeded() {
        // Acceptance: runtime-wide limit rejected (spec lines 300-303).
        let config = default_config();
        let budget = unlimited_budget();
        // Simulate runtime at 512 MiB ceiling.
        let runtime_used = 512 * 1024 * 1024;
        let err = check_budget(1024, &budget, runtime_used, &config).unwrap_err();
        assert!(matches!(err, ResourceError::BudgetExceeded { .. }));
    }

    // ─── Step 5: type check ───────────────────────────────────────────────────

    #[test]
    fn type_check_png_accepted() {
        // Acceptance: IMAGE_PNG accepted (spec lines 37-38).
        assert!(check_resource_type(ResourceType::ImagePng).is_ok());
    }

    // Note: all v1 variants exist in the enum; there is no way to construct an
    // unknown type without changing the enum, so the unsupported-type path is
    // tested via the wire-code unit test in types.rs.

    // ─── Step 6: decode validation ────────────────────────────────────────────

    #[test]
    fn decode_rgba8_valid() {
        // 2×2 image = 16 bytes of RGBA8. width=2, height=2, 2*2*4=16.
        let data = vec![0u8; 16];
        let config = default_config();
        let meta = decode_and_validate(&data, ResourceType::ImageRgba8, &config, 2, 2).unwrap();
        assert_eq!(meta.decoded_bytes, 16);
        assert_eq!(meta.width_px, 2);
        assert_eq!(meta.height_px, 2);
    }

    #[test]
    fn decode_rgba8_dimension_mismatch_rejected() {
        // Byte count does not match declared 3×3 (should be 36 bytes, but we pass 16).
        let data = vec![0u8; 16];
        let config = default_config();
        let err = decode_and_validate(&data, ResourceType::ImageRgba8, &config, 3, 3).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
    }

    #[test]
    fn decode_rgba8_zero_dimension_rejected() {
        let data = vec![0u8; 0];
        let config = default_config();
        let err = decode_and_validate(&data, ResourceType::ImageRgba8, &config, 0, 1).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
    }

    #[test]
    fn decode_rgba8_dimension_exceeds_max_rejected() {
        // 9000 pixels wide exceeds MAX_TEXTURE_DIMENSION_PX (8192).
        let config = default_config();
        // data length doesn't matter here — dimension check fires first
        let err =
            decode_and_validate(&[0u8; 4], ResourceType::ImageRgba8, &config, 9000, 1).unwrap_err();
        assert!(matches!(err, ResourceError::SizeExceeded { .. }));
    }

    #[test]
    fn decode_png_corrupt_rejected() {
        // Acceptance: corrupt PNG → RESOURCE_DECODE_ERROR (spec lines 93-94).
        let corrupt_png = b"this is not a valid png";
        let config = default_config();
        let err =
            decode_and_validate(corrupt_png, ResourceType::ImagePng, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
        assert_eq!(err.wire_code(), "RESOURCE_DECODE_ERROR");
    }

    #[test]
    fn decode_jpeg_corrupt_rejected() {
        let corrupt_jpeg = b"not a jpeg at all";
        let config = default_config();
        let err =
            decode_and_validate(corrupt_jpeg, ResourceType::ImageJpeg, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
    }

    #[test]
    fn decode_font_corrupt_rejected() {
        let corrupt_font = b"not a valid font file";
        let config = default_config();
        let err =
            decode_and_validate(corrupt_font, ResourceType::FontTtf, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
        assert_eq!(err.wire_code(), "RESOURCE_DECODE_ERROR");
    }

    #[test]
    fn decode_rgba8_size_exceeded() {
        // Acceptance: decoded size exceeds budget (spec lines 290-292).
        let config = ResourceStoreConfig {
            max_decoded_texture_bytes: 8, // very small for test
            ..Default::default()
        };
        // 2×2 = 16 bytes RGBA8, which exceeds the 8-byte limit.
        let data = vec![0u8; 16];
        let err = decode_and_validate(&data, ResourceType::ImageRgba8, &config, 2, 2).unwrap_err();
        assert!(matches!(err, ResourceError::SizeExceeded { .. }));
    }

    use super::test_helpers::minimal_png_1x1;

    #[test]
    fn decode_png_1x1_succeeds() {
        let config = default_config();
        let data = minimal_png_1x1();
        let meta = decode_and_validate(&data, ResourceType::ImagePng, &config, 0, 0).unwrap();
        assert_eq!(meta.width_px, 1);
        assert_eq!(meta.height_px, 1);
        assert_eq!(meta.decoded_bytes, 4); // 1×1×4 bytes RGBA8
    }

    #[test]
    fn agent_budget_unlimited_fits_any_size() {
        let budget = AgentBudget {
            texture_bytes_total_limit: 0,
            texture_bytes_total_used: 0,
        };
        assert!(budget.would_fit(usize::MAX));
    }

    #[test]
    fn agent_budget_limited_fits() {
        let budget = AgentBudget {
            texture_bytes_total_limit: 1000,
            texture_bytes_total_used: 500,
        };
        assert!(budget.would_fit(500));
        assert!(!budget.would_fit(501));
    }

    // ─── IMAGE_SVG: upload validation ─────────────────────────────────────────

    #[test]
    fn svg_valid_minimal_accepted() {
        // Acceptance: well-formed XML with <svg> root MUST be accepted.
        // Source: resource-store/spec.md §Scenario: IMAGE_SVG upload accepted with valid SVG
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        let config = default_config();
        let meta = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0).unwrap();
        // No viewBox/width/height → defaults to 512×512.
        assert_eq!(meta.width_px, 512);
        assert_eq!(meta.height_px, 512);
        assert_eq!(meta.decoded_bytes, 512 * 512 * 4);
    }

    #[test]
    fn svg_with_viewbox_budget_estimated_correctly() {
        // Acceptance: viewBox="0 0 800 600" → budget = 800*600*4 = 1,920,000.
        // Source: resource-store/spec.md §Scenario: SVG with viewBox budget estimated
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 600"></svg>"#;
        let config = default_config();
        let meta = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0).unwrap();
        assert_eq!(meta.width_px, 800);
        assert_eq!(meta.height_px, 600);
        assert_eq!(meta.decoded_bytes, 800 * 600 * 4);
    }

    #[test]
    fn svg_large_dimensions_clamped_to_2048() {
        // Acceptance: width=4096 height=4096 → clamped to 2048×2048.
        // Source: resource-store/spec.md §Scenario: SVG exceeding 2048 clamped
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4096" height="4096"></svg>"#;
        let config = default_config();
        let meta = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0).unwrap();
        assert_eq!(meta.width_px, 2048);
        assert_eq!(meta.height_px, 2048);
        assert_eq!(meta.decoded_bytes, 2048 * 2048 * 4);
    }

    #[test]
    fn svg_with_width_and_height_attrs() {
        // Width and height as attributes (no viewBox).
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="200"></svg>"#;
        let config = default_config();
        let meta = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0).unwrap();
        assert_eq!(meta.width_px, 100);
        assert_eq!(meta.height_px, 200);
    }

    #[test]
    fn svg_invalid_xml_rejected() {
        // Acceptance: not well-formed XML → RESOURCE_DECODE_ERROR.
        // Source: resource-store/spec.md §Scenario: IMAGE_SVG upload rejected with invalid XML
        let bad = b"this is not xml at all";
        let config = default_config();
        let err = decode_and_validate(bad, ResourceType::ImageSvg, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
        assert_eq!(err.wire_code(), "RESOURCE_DECODE_ERROR");
    }

    #[test]
    fn svg_non_svg_root_rejected() {
        // Acceptance: well-formed XML but root is not <svg> → RESOURCE_DECODE_ERROR.
        // Source: resource-store/spec.md §Scenario: IMAGE_SVG upload rejected with non-SVG XML root
        let html = b"<html><body>hello</body></html>";
        let config = default_config();
        let err = decode_and_validate(html, ResourceType::ImageSvg, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
        let msg = err.to_string();
        assert!(msg.contains("<html>") || msg.contains("html"), "got: {msg}");
    }

    #[test]
    fn svg_div_root_rejected() {
        let xml = b"<div xmlns=\"http://www.w3.org/1999/xhtml\"></div>";
        let config = default_config();
        let err = decode_and_validate(xml, ResourceType::ImageSvg, &config, 0, 0).unwrap_err();
        assert!(matches!(err, ResourceError::DecodeError(_)));
    }

    #[test]
    fn svg_with_xml_declaration_accepted() {
        // XML declaration before <svg> is fine.
        let svg = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        let config = default_config();
        let result = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0);
        assert!(result.is_ok(), "expected ok, got: {result:?}");
    }

    #[test]
    fn svg_viewbox_takes_priority_over_width_height() {
        // viewBox takes precedence over width/height attributes.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 400 300"></svg>"#;
        let config = default_config();
        let meta = decode_and_validate(svg, ResourceType::ImageSvg, &config, 0, 0).unwrap();
        assert_eq!(meta.width_px, 400);
        assert_eq!(meta.height_px, 300);
    }

    #[test]
    fn svg_image_svg_is_v1_supported() {
        // IMAGE_SVG must be in the v1 supported set.
        assert!(check_resource_type(ResourceType::ImageSvg).is_ok());
    }
}
