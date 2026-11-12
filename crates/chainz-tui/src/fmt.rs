//! Number, time and text formatting shared by the renderer and `--once`.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub use chainz_core::fmt::{group, short_address, uptime};

/// A coarse age: `4s`, `3m`, `2h`, `5d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..=99 => format!("{secs}s"),
        100..=5_999 => format!("{}m", secs / 60),
        6_000..=172_799 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// An average block time: `12.0s`, `2.0s`, `0.25s`.
pub fn block_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        "–".into()
    } else if secs < 0.995 {
        format!("{secs:.2}s")
    } else {
        format!("{secs:.1}s")
    }
}

/// A latency: `48ms`, `1.2s`.
pub fn latency(ms: u32) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 100_000 {
        format!("{:.1}s", f64::from(ms) / 1_000.0)
    } else {
        format!("{}s", ms / 1_000)
    }
}

/// A base fee in gwei with magnitude-adaptive precision: `31`, `4.2`,
/// `0.41`, `0.002`, `<0.001`.
pub fn gwei(wei: u128) -> String {
    let g = wei as f64 / 1e9;
    if wei == 0 {
        "0".into()
    } else if g >= 100.0 {
        format!("{g:.0}")
    } else if g >= 10.0 {
        format!("{g:.1}")
    } else if g >= 1.0 {
        format!("{g:.2}")
    } else if g >= 0.0005 {
        format!("{g:.3}")
    } else {
        "<0.001".into()
    }
}

/// A token amount with `decimals`, grouped, four fixed fraction digits so
/// a column lines up on the decimal point: `12,345.6789`, `0.0012`.
/// Zero is `0`; a non-zero amount below the last digit is `<0.0001`.
pub fn units(raw: u128, decimals: u8) -> String {
    const FRACTION: u32 = 4;
    if raw == 0 {
        return "0".into();
    }
    if raw == u128::MAX {
        return ">3.4e38".into();
    }
    let decimals = u32::from(decimals);
    let Some(scale) = 10u128.checked_pow(decimals) else {
        return "<0.0001".into();
    };
    let whole = raw / scale;
    let rest = raw % scale;
    let fraction = if decimals >= FRACTION {
        rest / 10u128.pow(decimals - FRACTION)
    } else {
        rest * 10u128.pow(FRACTION - decimals)
    };
    if whole == 0 && fraction == 0 {
        return "<0.0001".into();
    }
    let whole = u64::try_from(whole).map_or_else(|_| whole.to_string(), group);
    format!("{whole}.{fraction:0width$}", width = FRACTION as usize)
}

/// The exact decimal string of `raw / 10^decimals` (for JSON).
pub fn units_exact(raw: u128, decimals: u8) -> String {
    let digits = raw.to_string();
    let decimals = usize::from(decimals);
    if decimals == 0 {
        return digits;
    }
    let padded = format!("{digits:0>width$}", width = decimals + 1);
    let (whole, fraction) = padded.split_at(padded.len() - decimals);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// Display width in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut `s` to at most `max` cells, ending in `…` when anything was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One sparkline glyph per sample (the last `width` of them); `None`
/// samples (failures) come back as `None` so the caller can style them.
///
/// The scale runs from the smallest sample to at least 2.5× it, so a steady
/// endpoint draws flat and only real spikes stand out.
pub fn sparkline(samples: &[Option<u32>], width: usize) -> Vec<Option<char>> {
    let tail = &samples[samples.len().saturating_sub(width)..];
    let ok = tail.iter().flatten().copied();
    let lo = f64::from(ok.clone().min().unwrap_or(0));
    let hi = f64::from(ok.max().unwrap_or(0)).max(lo * 2.5).max(lo + 1.0);
    tail.iter()
        .map(|s| {
            s.map(|v| {
                let level = ((f64::from(v) - lo) / (hi - lo) * 7.0).round();
                BARS[(level as usize).min(7)]
            })
        })
        .collect()
}

/// Standard base64 with padding (for OSC 52).
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(age(4), "4s");
        assert_eq!(age(99), "99s");
        assert_eq!(age(100), "1m");
        assert_eq!(age(7_200), "2h");
        assert_eq!(age(400_000), "4d");
    }

    #[test]
    fn block_times_and_latencies() {
        assert_eq!(block_time(12.04), "12.0s");
        assert_eq!(block_time(0.25), "0.25s");
        assert_eq!(block_time(0.999), "1.0s");
        assert_eq!(block_time(f64::NAN), "–");
        assert_eq!(latency(48), "48ms");
        assert_eq!(latency(1_240), "1.2s");
    }

    #[test]
    fn gwei_precision_follows_magnitude() {
        assert_eq!(gwei(0), "0");
        assert_eq!(gwei(410_000_000), "0.410");
        assert_eq!(gwei(2_000_000), "0.002");
        assert_eq!(gwei(7), "<0.001");
        assert_eq!(gwei(4_230_000_000), "4.23");
        assert_eq!(gwei(31_400_000_000), "31.4");
        assert_eq!(gwei(131_400_000_000), "131");
    }

    #[test]
    fn units_use_the_token_decimals() {
        assert_eq!(units(0, 18), "0");
        assert_eq!(units(1_234_500_000_000_000_000, 18), "1.2345");
        assert_eq!(units(12_345_678_900, 6), "12,345.6789");
        assert_eq!(units(1_200, 6), "0.0012");
        assert_eq!(units(1, 18), "<0.0001");
        assert_eq!(units(15, 1), "1.5000");
        assert_eq!(units(7, 0), "7.0000");
        assert_eq!(units(u128::MAX, 18), ">3.4e38");
        assert_eq!(units_exact(1_234_500_000_000_000_000, 18), "1.2345");
        assert_eq!(units_exact(1, 18), "0.000000000000000001");
        assert_eq!(units_exact(5_000_000, 6), "5");
        assert_eq!(units_exact(42, 0), "42");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("infura", 10), "infura");
        assert_eq!(truncate("publicnode-2", 8), "publicn…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(
            short_address(
                &"0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
                    .parse()
                    .unwrap()
            ),
            "0x7E5F…5Bdf"
        );
    }

    #[test]
    fn sparkline_is_flat_when_steady_and_marks_failures() {
        let steady: Vec<Option<u32>> = vec![Some(50), Some(52), Some(49), Some(51)];
        assert!(sparkline(&steady, 8).iter().all(|c| *c == Some('▁')));
        let spiky = [Some(50), Some(50), None, Some(400)];
        let line = sparkline(&spiky, 3);
        assert_eq!(line, vec![Some('▁'), None, Some('█')]);
        assert!(sparkline(&[], 8).is_empty());
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }
}
