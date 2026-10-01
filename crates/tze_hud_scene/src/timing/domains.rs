//! Clock-domain newtype wrappers.
//!
//! Compile-time type-safe wrappers for the two clock domains that appear in
//! Rust/proto fields:
//!
//! | Wrapper       | Domain         | Field suffix | Unit                         |
//! |---------------|----------------|--------------|------------------------------|
//! | [`WallUs`]    | Network (UTC)  | `_wall_us`   | UTC microseconds since epoch |
//! | [`MonoUs`]    | Monotonic OS   | `_mono_us`   | Monotonic microseconds       |
//! | [`DurationUs`]| —              | *(delta)*    | Microsecond delta            |
//!
//! `WallUs` and `MonoUs` are **not interchangeable**: passing one where the
//! other is expected is a compile-time error.
//!
//! ## Zero-value semantics
//!
//! A timestamp of `0` means "not set".
//! [`WallUs::is_set`] and [`MonoUs::is_set`] encode this convention.
//!
//! ## Field naming convention
//!
//! All timestamp fields in proto and Rust structs MUST encode their domain in
//! the suffix:
//! - `_wall_us` — use [`WallUs`]
//! - `_mono_us` — use [`MonoUs`]
//! - no domain suffix — use [`DurationUs`] (delta / frame-relative, not a
//!   timestamp)
//!
//! A plain `_us` suffix without domain indicator MUST NOT be used for
//! absolute timestamps.

use serde::{Deserialize, Serialize};

// ─── WallUs ──────────────────────────────────────────────────────────────────

/// UTC wall-clock timestamp in microseconds since the Unix epoch.
///
/// Corresponds to the *network clock domain* and MUST be used for fields
/// with the `_wall_us` suffix (e.g. `present_at_wall_us`, `created_at_wall_us`,
/// `session_open_wall_us`).
///
/// # Zero semantics
///
/// `WallUs(0)` means "not set" (spec lines 68–70).  Production clocks MUST NOT
/// return 0.  Use [`WallUs::NOT_SET`] as the canonical sentinel.
///
/// # Cross-domain assignment is a compile error
///
/// ```compile_fail
/// use tze_hud_scene::timing::{WallUs, MonoUs};
///
/// let wall: WallUs = WallUs(1_000_000);
/// let mono: MonoUs = wall; // ERROR: mismatched types
/// ```
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct WallUs(pub u64);

impl WallUs {
    /// Sentinel value meaning "not set".  Always `WallUs(0)`.
    pub const NOT_SET: Self = Self(0);

    /// `true` if this timestamp carries a real value (i.e. is non-zero).
    #[inline]
    pub fn is_set(self) -> bool {
        self.0 != 0
    }

    /// Raw microsecond value.
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for WallUs {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<WallUs> for u64 {
    fn from(v: WallUs) -> Self {
        v.0
    }
}

impl std::fmt::Display for WallUs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}µs(wall)", self.0)
    }
}

// ─── MonoUs ──────────────────────────────────────────────────────────────────

/// Monotonic system-clock timestamp in microseconds.
///
/// Corresponds to the *monotonic clock domain* and MUST be used for fields
/// with the `_mono_us` suffix (e.g. `vsync_mono_us`, `session_open_mono_us`,
/// `timestamp_mono_us`).
///
/// Monotonic values MUST NOT be compared directly with wall-clock values.
///
/// # Zero semantics
///
/// `MonoUs(0)` means "not set".  Use [`MonoUs::NOT_SET`] as the sentinel.
///
/// # Cross-domain assignment is a compile error
///
/// ```compile_fail
/// use tze_hud_scene::timing::{MonoUs, WallUs};
///
/// let mono: MonoUs = MonoUs(5_000_000);
/// let wall: WallUs = mono; // ERROR: mismatched types
/// ```
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct MonoUs(pub u64);

impl MonoUs {
    /// Sentinel value meaning "not set".  Always `MonoUs(0)`.
    pub const NOT_SET: Self = Self(0);

    /// `true` if this timestamp carries a real value (i.e. is non-zero).
    #[inline]
    pub fn is_set(self) -> bool {
        self.0 != 0
    }

    /// Raw microsecond value.
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for MonoUs {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<MonoUs> for u64 {
    fn from(v: MonoUs) -> Self {
        v.0
    }
}

impl std::fmt::Display for MonoUs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}µs(mono)", self.0)
    }
}

// ─── DurationUs ──────────────────────────────────────────────────────────────

/// A duration (delta) in microseconds — NOT a timestamp.
///
/// Use this type for fields that express an interval or offset rather than an
/// absolute point in time.  Such fields MUST NOT carry a `_wall_us` or
/// `_mono_us` suffix; they use a plain unit description (e.g. `after_us`,
/// `duration_us`, `ttl_us`).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct DurationUs(pub u64);

impl DurationUs {
    /// Zero duration.
    pub const ZERO: Self = Self(0);

    /// Raw microsecond value.
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Add this duration to a [`WallUs`] timestamp.
    #[inline]
    pub fn after_wall(self, base: WallUs) -> WallUs {
        WallUs(base.0.saturating_add(self.0))
    }

    /// Add this duration to a [`MonoUs`] timestamp.
    #[inline]
    pub fn after_mono(self, base: MonoUs) -> MonoUs {
        MonoUs(base.0.saturating_add(self.0))
    }
}

impl From<u64> for DurationUs {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<DurationUs> for u64 {
    fn from(v: DurationUs) -> Self {
        v.0
    }
}

impl std::fmt::Display for DurationUs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}µs", self.0)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── WallUs ──

    #[test]
    fn wall_us_not_set_is_zero() {
        assert_eq!(WallUs::NOT_SET.0, 0);
        assert!(!WallUs::NOT_SET.is_set());
    }

    #[test]
    fn wall_us_nonzero_is_set() {
        assert!(WallUs(1).is_set());
        assert!(WallUs(u64::MAX).is_set());
    }

    #[test]
    fn wall_us_from_u64() {
        let v: WallUs = WallUs::from(42_000_000);
        assert_eq!(v.as_u64(), 42_000_000);
    }

    #[test]
    fn wall_us_roundtrip_u64() {
        let v = WallUs(999);
        assert_eq!(u64::from(v), 999);
    }

    #[test]
    fn wall_us_display() {
        assert_eq!(format!("{}", WallUs(1_500_000)), "1500000µs(wall)");
    }

    // ── MonoUs ──

    #[test]
    fn mono_us_not_set_is_zero() {
        assert_eq!(MonoUs::NOT_SET.0, 0);
        assert!(!MonoUs::NOT_SET.is_set());
    }

    #[test]
    fn mono_us_nonzero_is_set() {
        assert!(MonoUs(1).is_set());
    }

    #[test]
    fn mono_us_display() {
        assert_eq!(format!("{}", MonoUs(2_000_000)), "2000000µs(mono)");
    }

    // ── DurationUs ──

    #[test]
    fn duration_us_zero() {
        assert_eq!(DurationUs::ZERO.as_u64(), 0);
    }

    #[test]
    fn duration_after_wall() {
        let base = WallUs(1_000_000);
        let delta = DurationUs(500_000);
        assert_eq!(delta.after_wall(base), WallUs(1_500_000));
    }

    #[test]
    fn duration_after_mono() {
        let base = MonoUs(2_000_000);
        let delta = DurationUs(100_000);
        assert_eq!(delta.after_mono(base), MonoUs(2_100_000));
    }

    #[test]
    fn duration_after_wall_saturates_on_overflow() {
        let base = WallUs(u64::MAX);
        let delta = DurationUs(1);
        assert_eq!(delta.after_wall(base), WallUs(u64::MAX));
    }

    // ── Spec: cross-domain assignment is a compile error ──
    // The tests below are compile_fail doc-tests in the struct documentation.
    // They are not repeated here because they cannot be written as #[test].

    // ── Zero-value semantics ──

    #[test]
    fn zero_means_not_set_wall() {
        // present_at_wall_us = 0 → "not set" / immediate
        let ts = WallUs(0);
        assert!(!ts.is_set(), "0 must mean 'not set'");
    }

    #[test]
    fn zero_means_not_set_mono() {
        let ts = MonoUs(0);
        assert!(!ts.is_set(), "0 must mean 'not set'");
    }

    // ── Ordering ──

    #[test]
    fn wall_us_ordering() {
        assert!(WallUs(100) < WallUs(200));
        assert!(WallUs(200) > WallUs(100));
        assert!(WallUs(100) == WallUs(100));
    }

    #[test]
    fn mono_us_ordering() {
        assert!(MonoUs(50) < MonoUs(51));
    }
}
