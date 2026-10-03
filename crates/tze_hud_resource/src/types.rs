//! Core types for the resource store: ResourceId, ResourceType, and error codes.
//!
//! # ResourceId
//!
//! A `ResourceId` is the 32-byte (256-bit) BLAKE3 digest of the raw input bytes
//! *before* any decode or transcode.  Two uploads of identical bytes always
//! produce the same `ResourceId` regardless of the uploading agent's identity.
//!
//! **Wire format**: raw 32 bytes in a protobuf `bytes` field.
//! **Log/CLI format**: lowercase hex string (64 hex chars).
//!
//! Source: RFC 0011 §1.1, §1.4; resource-store/spec.md lines 5-8.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── ResourceId ──────────────────────────────────────────────────────────────

/// Content-addressed identifier: 32-byte BLAKE3 digest of raw input bytes.
///
/// Equality, ordering, and hashing are defined over the raw bytes — no
/// normalization is applied.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceId([u8; 32]);

impl ResourceId {
    /// Wrap a raw 32-byte digest.
    #[inline]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw 32-byte digest.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compute a `ResourceId` from a byte slice using BLAKE3.
    ///
    /// BLAKE3 achieves ~3 GB/s on modern hardware; 1 MiB hashes in <1 ms
    /// (spec line 330).
    #[inline]
    pub fn from_content(bytes: &[u8]) -> Self {
        let hash = blake3::hash(bytes);
        Self(*hash.as_bytes())
    }

    /// Attempt to parse from a 32-byte slice (e.g., from a protobuf `bytes` field).
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let arr: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(arr))
    }

    /// Lowercase hex string for logging and CLI display.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write;
            write!(s, "{b:02x}").expect("write to String is infallible");
        }
        s
    }
}

impl std::fmt::Debug for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResourceId({})", self.to_hex())
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// ─── ResourceType ─────────────────────────────────────────────────────────────

/// V1 resource types supported by the runtime.
///
/// Six types are supported in v1: five raster/font types and one vector type
/// (IMAGE_SVG).  Uploads of any other type are rejected with
/// `ResourceError::UnsupportedType`.
///
/// Source: resource-store/spec.md §Requirement: V1 Resource Type Enumeration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceType {
    /// Raw RGBA8 pixel data (width × height × 4 bytes, row-major, top-left origin).
    ImageRgba8,
    /// PNG-encoded image.
    ImagePng,
    /// JPEG-encoded image.
    ImageJpeg,
    /// TrueType font.
    FontTtf,
    /// OpenType font.
    FontOtf,
    /// SVG vector image.
    ///
    /// Used exclusively by the widget asset bundle system.  IMAGE_SVG resources
    /// SHALL NOT be publishable directly to zones.
    ///
    /// Upload validation MUST verify well-formed XML with an `<svg>` root element.
    /// No rasterization occurs at upload time.
    ///
    /// Budget accounting uses an estimated rasterized size:
    /// `width_px * height_px * 4` where dimensions come from `viewBox` or
    /// `width`/`height` attributes, with a 512×512 default and 2048×2048 clamp.
    ///
    /// Source: resource-store/spec.md §Requirement: V1 Resource Type Enumeration,
    ///         §Requirement: SVG Resource Budget Accounting.
    ImageSvg,
}

impl ResourceType {
    /// Return `true` iff this type is part of the v1 supported set.
    ///
    /// All six variants are v1-supported; the method exists so callers can
    /// express intent clearly and so future post-v1 variants (VIDEO_H264, etc.)
    /// can be added without changing call sites.
    #[inline]
    pub fn is_v1_supported(self) -> bool {
        // All current variants are v1; post-v1 types would map to a separate
        // enum or a discriminant > the current max.
        matches!(
            self,
            ResourceType::ImageRgba8
                | ResourceType::ImagePng
                | ResourceType::ImageJpeg
                | ResourceType::FontTtf
                | ResourceType::FontOtf
                | ResourceType::ImageSvg
        )
    }
}

impl std::fmt::Display for ResourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ResourceType::ImageRgba8 => "IMAGE_RGBA8",
            ResourceType::ImagePng => "IMAGE_PNG",
            ResourceType::ImageJpeg => "IMAGE_JPEG",
            ResourceType::FontTtf => "FONT_TTF",
            ResourceType::FontOtf => "FONT_OTF",
            ResourceType::ImageSvg => "IMAGE_SVG",
        };
        f.write_str(s)
    }
}

// ─── SVG budget constants ──────────────────────────────────────────────────────

/// Default SVG rasterized dimension when no explicit `viewBox`/`width`/`height`
/// is present (512 × 512 pixels).
///
/// Source: resource-store/spec.md §Requirement: SVG Resource Budget Accounting.
pub const SVG_DEFAULT_DIMENSION_PX: u32 = 512;

/// Maximum SVG rasterized dimension for budget clamping (2048 × 2048).
///
/// Source: resource-store/spec.md §Requirement: SVG Resource Budget Accounting.
pub const SVG_MAX_DIMENSION_PX: u32 = 2048;

// ─── Resource size limits (configurable via ResourceStoreConfig) ──────────────

/// Default maximum raw upload size per resource (16 MiB).
pub const DEFAULT_MAX_RESOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum decoded texture size (64 MiB).
pub const DEFAULT_MAX_DECODED_TEXTURE_BYTES: usize = 64 * 1024 * 1024;

/// Maximum texture dimension in either axis (not configurable per spec).
pub const MAX_TEXTURE_DIMENSION_PX: u32 = 8192;

/// Default maximum total texture memory across all agents (512 MiB).
pub const DEFAULT_MAX_TOTAL_TEXTURE_BYTES: usize = 512 * 1024 * 1024;

/// Default maximum number of concurrent resources in the store.
pub const DEFAULT_MAX_CONCURRENT_RESOURCES: usize = 4096;

/// Maximum chunk size for chunked uploads (64 KiB).
pub const CHUNK_SIZE_LIMIT: usize = 64 * 1024;

/// Resources <= this size may use the inline fast path.
pub const INLINE_SIZE_LIMIT: usize = 64 * 1024;

/// Maximum concurrent uploads per agent.
pub const MAX_CONCURRENT_UPLOADS_PER_AGENT: usize = 4;

/// Default per-agent upload rate limit (1 MiB/s).
pub const DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC: usize = 1024 * 1024;

// ─── Decoded resource dimensions (for texture budget accounting) ──────────────

/// Metadata produced by the decode validation step.
#[derive(Clone, Debug)]
pub struct DecodedMeta {
    /// Decoded in-memory size in bytes (used for budget accounting).
    pub decoded_bytes: usize,
    /// Width in pixels (images only; 0 for fonts).
    pub width_px: u32,
    /// Height in pixels (images only; 0 for fonts).
    pub height_px: u32,
}

// ─── Resource store configuration ─────────────────────────────────────────────

/// Runtime-configurable limits for the resource store.
///
/// All fields have defaults that match the spec's defaults.  Operators can
/// override these on construction.
#[derive(Clone, Debug)]
pub struct ResourceStoreConfig {
    /// Maximum raw upload bytes per resource (default 16 MiB).
    pub max_resource_bytes: usize,
    /// Maximum decoded texture bytes per resource (default 64 MiB).
    pub max_decoded_texture_bytes: usize,
    /// Maximum total texture bytes across all resources (default 512 MiB).
    pub max_total_texture_bytes: usize,
    /// Maximum number of concurrent resources in the store (default 4096).
    pub max_concurrent_resources: usize,
    /// Per-agent upload rate limit in bytes/second (default 1 MiB/s).
    ///
    /// Enforced by the session transport layer, which applies stream-level
    /// backpressure to resident upload intake.
    pub upload_rate_limit_bytes_per_sec: usize,
}

impl Default for ResourceStoreConfig {
    fn default() -> Self {
        Self {
            max_resource_bytes: DEFAULT_MAX_RESOURCE_BYTES,
            max_decoded_texture_bytes: DEFAULT_MAX_DECODED_TEXTURE_BYTES,
            max_total_texture_bytes: DEFAULT_MAX_TOTAL_TEXTURE_BYTES,
            max_concurrent_resources: DEFAULT_MAX_CONCURRENT_RESOURCES,
            upload_rate_limit_bytes_per_sec: DEFAULT_UPLOAD_RATE_LIMIT_BYTES_PER_SEC,
        }
    }
}

// ─── Error codes ──────────────────────────────────────────────────────────────

/// Resource store errors; each maps 1-to-1 with a spec-defined error code.
///
/// Source: RFC 0011 §3.5, resource-store/spec.md lines 83-98.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ResourceError {
    /// Agent does not hold the `upload_resource` capability.
    /// Wire code: `RESOURCE_CAPABILITY_DENIED`.
    #[error("RESOURCE_CAPABILITY_DENIED: agent lacks upload_resource capability")]
    CapabilityDenied,

    /// Computed BLAKE3 hash does not match `expected_hash`.
    /// Wire code: `RESOURCE_HASH_MISMATCH`.
    #[error("RESOURCE_HASH_MISMATCH: computed hash {computed} ≠ expected {expected}")]
    HashMismatch { computed: String, expected: String },

    /// Raw upload size exceeds `max_resource_bytes`, or decoded texture exceeds
    /// `max_decoded_texture_bytes`, or a texture dimension exceeds 8192 px.
    /// Wire code: `RESOURCE_SIZE_EXCEEDED`.
    #[error("RESOURCE_SIZE_EXCEEDED: {detail}")]
    SizeExceeded { detail: String },

    /// Agent's `texture_bytes_total` lease budget is exhausted, or the
    /// runtime-wide texture memory ceiling is reached.
    /// Wire code: `RESOURCE_BUDGET_EXCEEDED`.
    #[error("RESOURCE_BUDGET_EXCEEDED: {detail}")]
    BudgetExceeded { detail: String },

    /// `resource_type` is not in the v1 supported set.
    /// Wire code: `RESOURCE_UNSUPPORTED_TYPE`.
    #[error("RESOURCE_UNSUPPORTED_TYPE: type {0} is not supported in v1")]
    UnsupportedType(String),

    /// Content could not be decoded (corrupt PNG/JPEG, invalid font, etc.).
    /// Wire code: `RESOURCE_DECODE_ERROR`.
    #[error("RESOURCE_DECODE_ERROR: {0}")]
    DecodeError(String),

    /// Agent already has `MAX_CONCURRENT_UPLOADS_PER_AGENT` uploads in flight.
    /// Wire code: `RESOURCE_TOO_MANY_UPLOADS`.
    #[error("RESOURCE_TOO_MANY_UPLOADS: agent has too many concurrent uploads")]
    TooManyUploads,

    /// Chunk arrived out-of-order or for an unknown upload ID.
    #[error("RESOURCE_INVALID_CHUNK: {0}")]
    InvalidChunk(String),

    /// Upload was aborted before completion.
    ///
    /// **Internal use only** — not transmitted on the wire. Used for
    /// in-process error propagation when a session disconnects mid-upload.
    #[error("RESOURCE_UPLOAD_ABORTED: {0}")]
    UploadAborted(String),

    /// Internal store error.
    ///
    /// **Internal use only** — not transmitted on the wire. Used for
    /// unexpected runtime conditions that do not map to a spec error code.
    #[error("RESOURCE_INTERNAL: {0}")]
    Internal(String),
}

impl ResourceError {
    /// Stable wire code string (used in protobuf `error_code` fields).
    pub fn wire_code(&self) -> &'static str {
        match self {
            ResourceError::CapabilityDenied => "RESOURCE_CAPABILITY_DENIED",
            ResourceError::HashMismatch { .. } => "RESOURCE_HASH_MISMATCH",
            ResourceError::SizeExceeded { .. } => "RESOURCE_SIZE_EXCEEDED",
            ResourceError::BudgetExceeded { .. } => "RESOURCE_BUDGET_EXCEEDED",
            ResourceError::UnsupportedType(_) => "RESOURCE_UNSUPPORTED_TYPE",
            ResourceError::DecodeError(_) => "RESOURCE_DECODE_ERROR",
            ResourceError::TooManyUploads => "RESOURCE_TOO_MANY_UPLOADS",
            ResourceError::InvalidChunk(_) => "RESOURCE_INVALID_CHUNK",
            ResourceError::UploadAborted(_) => "RESOURCE_UPLOAD_ABORTED",
            ResourceError::Internal(_) => "RESOURCE_INTERNAL",
        }
    }
}

// ─── Upload result ────────────────────────────────────────────────────────────

/// Successful result returned to the agent after an upload completes.
#[derive(Clone, Debug)]
pub struct ResourceStored {
    /// The content-addressed identifier for this resource.
    pub resource_id: ResourceId,
    /// `true` if the resource already existed and no new storage was consumed.
    pub was_deduplicated: bool,
    /// Decoded in-memory size in bytes (used for budget accounting reporting).
    pub decoded_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_from_content_is_32_bytes() {
        // Acceptance: ResourceId MUST be exactly 32 bytes (spec line 14-16).
        let id = ResourceId::from_content(b"hello world");
        assert_eq!(id.as_bytes().len(), 32);
    }

    #[test]
    fn identical_bytes_produce_identical_resource_id() {
        // Acceptance: two uploads of identical bytes MUST produce same ResourceId
        // (spec lines 11-12).
        let data = b"some image data";
        let id_a = ResourceId::from_content(data);
        let id_b = ResourceId::from_content(data);
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn different_bytes_produce_different_resource_id() {
        let id_a = ResourceId::from_content(b"aaa");
        let id_b = ResourceId::from_content(b"bbb");
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn resource_id_hex_is_64_chars() {
        let id = ResourceId::from_content(b"test");
        assert_eq!(id.to_hex().len(), 64);
        // All chars must be lowercase hex.
        assert!(id.to_hex().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resource_id_from_slice_roundtrip() {
        let id = ResourceId::from_content(b"roundtrip");
        let bytes_slice: &[u8] = id.as_bytes();
        let id2 = ResourceId::from_slice(bytes_slice).unwrap();
        assert_eq!(id, id2);
    }

    #[test]
    fn resource_id_from_slice_wrong_length_returns_none() {
        assert!(ResourceId::from_slice(&[0u8; 31]).is_none());
        assert!(ResourceId::from_slice(&[0u8; 33]).is_none());
        assert!(ResourceId::from_slice(&[]).is_none());
    }

    #[test]
    fn all_v1_types_are_supported() {
        // Acceptance: all six v1 types must be supported.
        // Source: resource-store/spec.md §Requirement: V1 Resource Type Enumeration.
        for rt in [
            ResourceType::ImageRgba8,
            ResourceType::ImagePng,
            ResourceType::ImageJpeg,
            ResourceType::FontTtf,
            ResourceType::FontOtf,
            ResourceType::ImageSvg,
        ] {
            assert!(rt.is_v1_supported(), "{rt} should be v1-supported");
        }
    }

    #[test]
    fn image_svg_display_name() {
        assert_eq!(ResourceType::ImageSvg.to_string(), "IMAGE_SVG");
    }

    #[test]
    fn wire_codes_are_stable() {
        assert_eq!(
            ResourceError::CapabilityDenied.wire_code(),
            "RESOURCE_CAPABILITY_DENIED"
        );
        assert_eq!(
            ResourceError::TooManyUploads.wire_code(),
            "RESOURCE_TOO_MANY_UPLOADS"
        );
        assert_eq!(
            ResourceError::UnsupportedType("VIDEO_H264".into()).wire_code(),
            "RESOURCE_UNSUPPORTED_TYPE"
        );
    }
}
