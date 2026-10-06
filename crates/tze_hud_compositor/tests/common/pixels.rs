//! Pure RGBA sampling/assertion helpers shared only by existing test binaries.
//! No GPU setup or runtime dependencies; preserve the original surface helper semantics.

/// Get pixel at (x, y) from raw RGBA data. Returns [r, g, b, a].
pub fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

/// Assert that a pixel at (x, y) is within `tolerance` of `expected` on
/// every channel (R, G, B, A).
///
/// Returns `Ok([r, g, b, a])` on pass, `Err(message)` on failure.
///
/// The `label` is included in the error message for diagnostic clarity.
///
/// Software-rasterised GPU paths (llvmpipe / SwiftShader) may produce
/// values that differ from the linear-space input by ±2 per channel due
/// to sRGB conversion.  Use `tolerance = 2` for solid fills on CI.
pub fn assert_pixel_color(
    data: &[u8],
    width: u32,
    x: u32,
    y: u32,
    expected: [u8; 4],
    tolerance: u8,
    label: &str,
) -> Result<[u8; 4], String> {
    let actual = pixel_at(data, width, x, y);
    for ch in 0..4 {
        let diff = actual[ch].abs_diff(expected[ch]);
        if diff > tolerance {
            return Err(format!(
                "pixel assertion failed at ({x},{y}) [{label}]: \
                 channel {ch} actual={} expected={} diff={} tolerance={}",
                actual[ch], expected[ch], diff, tolerance,
            ));
        }
    }
    Ok(actual)
}
