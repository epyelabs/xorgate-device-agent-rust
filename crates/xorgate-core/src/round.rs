//! JS-exact numeric helpers.
//!
//! The TS agent rounds every published float with
//! `Math.round(v * 10 ** decimals) / 10 ** decimals`
//! (`src/agent/collectors/system.ts:117-120` and the same helper in every
//! other collector). Two details matter for byte-identical output:
//!
//! 1. `Math.round` rounds half-way cases toward +∞ (`Math.round(-2.5) === -2`,
//!    `Math.round(2.5) === 3`), whereas Rust's `f64::round` rounds half away
//!    from zero (`(-2.5f64).round() == -3.0`). [`js_math_round`] implements the
//!    ECMAScript semantics.
//! 2. `10 ** n` for small non-negative integer `n` is exact in both languages
//!    (`powi` is exact for these magnitudes), so the scale factor is identical.

/// ECMAScript `Math.round(x)`: the integer closest to `x`, ties toward +∞.
///
/// NaN/±∞ pass through unchanged, exactly like JS.
pub fn js_math_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

/// Port of the TS `round(v, decimals)` helper: `Math.round(v * f) / f` with
/// `f = 10 ** decimals`.
pub fn round(v: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    js_math_round(v * f) / f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_round_ties_go_up() {
        assert_eq!(js_math_round(2.5), 3.0);
        assert_eq!(js_math_round(-2.5), -2.0);
        assert_eq!(js_math_round(-2.6), -3.0);
        assert_eq!(js_math_round(0.49999999999999994), 0.0);
        assert_eq!(js_math_round(-0.4), 0.0);
        assert!(js_math_round(f64::NAN).is_nan());
    }

    #[test]
    fn round_matches_the_ts_helper() {
        assert_eq!(round(5.84999, 1), 5.8);
        assert_eq!(round(5.85, 1), 5.9); // 58.5 → 59 (tie up), then /10
        assert_eq!(round(52.9, 1), 52.9);
        assert_eq!(round(5.13756, 3), 5.138);
        assert_eq!(round(4.9385, 3), 4.939); // 4938.5 → 4939
        assert_eq!(round(0.0, 1), 0.0);
        assert_eq!(round(100.0, 1), 100.0);
    }
}
