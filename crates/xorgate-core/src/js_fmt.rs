//! JavaScript string conversions that consumer-visible strings depend on.
//!
//! The TS deciders build their event/reason strings with template literals
//! (`${cfg.railLowVolts}V`, `${lastRail?.toFixed(2)}V`, …), so byte-identical
//! output needs ECMAScript `Number::toString` and `Number.prototype.toFixed`
//! semantics rather than Rust's `Display`. Both differ from Rust in the
//! half-way case: `toFixed` picks the LARGER candidate on an exact tie
//! (`(4.625).toFixed(2) === "4.63"`, `(0.125).toFixed(2) === "0.13"`), where
//! Rust's `{:.2}` rounds half to even. Verified against node 24 on 2026-10-09
//! (plans/device-agent-js-to-rust/phase-1/evidence/wp3.md).

/// ECMAScript `Number.prototype.toString()` (radix 10) for the magnitudes the
/// agent prints (volts, °C, mA, percentages, ms).
///
/// Integer-valued finite numbers print without a fraction (`80` → `"80"`,
/// `-0` → `"0"`); other values in `[1e-6, 1e21)` print the shortest
/// round-trip decimal, which is what Rust's `Display` produces too; outside
/// that range JS switches to exponent form (`1e+21`, `1e-7`).
pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    let a = v.abs();
    if a >= 1e21 || a < 1e-6 {
        // Rust prints `1e21` / `1.5e-7`; JS prints `1e+21` / `1.5e-7`.
        let s = format!("{v:e}");
        return if s.contains("e-") {
            s
        } else {
            s.replacen('e', "e+", 1)
        };
    }
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// ECMAScript `Number.prototype.toFixed(digits)` for finite `x` with
/// `|x| < 1e21`: the integer `n` minimising `|n / 10^digits - x|`, ties
/// resolved toward the larger `n` (computed exactly from the double's
/// mantissa/exponent, so no intermediate rounding can flip a tie).
///
/// A negative `x` keeps its sign even when the digits are all zero
/// (`(-0.001).toFixed(2) === "-0.00"`); `-0` prints as `"0.00"`.
pub fn to_fixed(x: f64, digits: u32) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return number_to_string(x);
    }
    let neg = x < 0.0;
    let ax = x.abs();
    let bits = ax.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    // ax = m * 2^e exactly.
    let (m, e) = if exp == 0 {
        (frac, -1074)
    } else {
        (frac | (1u64 << 52), exp - 1075)
    };
    let scale = 10u128.pow(digits);
    let n: u128 = if e >= 0 {
        // ax < 1e21 < 2^70, so m * 2^e * 10^digits fits comfortably for the
        // digit counts the agent uses.
        (m as u128) * scale << e
    } else {
        let sh = (-e) as u32;
        let num = (m as u128) * scale;
        if sh >= 128 {
            0
        } else {
            let half = 1u128 << (sh - 1);
            (num + half) >> sh
        }
    };
    let mut s = n.to_string();
    if digits > 0 {
        let d = digits as usize;
        if s.len() <= d {
            s = format!("{}{}", "0".repeat(d + 1 - s.len()), s);
        }
        s.insert(s.len() - d, '.');
    }
    if neg {
        s.insert(0, '-');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_matches_v8() {
        // Probed in node 24 (evidence/wp3.md).
        assert_eq!(number_to_string(4.85), "4.85");
        assert_eq!(number_to_string(80.0), "80");
        assert_eq!(number_to_string(5000.0), "5000");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(70.0), "70");
        assert_eq!(number_to_string(f64::NAN), "NaN");
    }

    #[test]
    fn to_fixed_matches_v8() {
        // Probed in node 24 (evidence/wp3.md): ties go to the larger n.
        for (x, two, one) in [
            (4.625, "4.63", "4.6"),
            (4.875, "4.88", "4.9"),
            (4.805, "4.80", "4.8"),
            (4.8, "4.80", "4.8"),
            (4.7, "4.70", "4.7"),
            (5.0, "5.00", "5.0"),
            (0.125, "0.13", "0.1"),
            (-0.001, "-0.00", "-0.0"),
            (-0.0, "0.00", "0.0"),
            (2.5, "2.50", "2.5"),
            (1.005, "1.00", "1.0"),
            (4.995, "5.00", "5.0"),
            (4.845, "4.84", "4.8"),
            (85.0, "85.00", "85.0"),
            (0.0, "0.00", "0.0"),
        ] {
            assert_eq!(to_fixed(x, 2), two, "{x}.toFixed(2)");
            assert_eq!(to_fixed(x, 1), one, "{x}.toFixed(1)");
        }
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(1e-30, 2), "0.00");
        assert_eq!(to_fixed(123456.0, 0), "123456");
    }
}
