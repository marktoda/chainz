//! Number, time and text formatting shared by the renderer and `--once`.

use chainz_core::fmt::group;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// A window length: `1m`, `15m`, `90s`.
pub fn window(secs: u64) -> String {
    if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
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
pub fn gwei(g: f64) -> String {
    if g == 0.0 {
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

/// Display width in terminal cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest start of `s` that fits in `max` cells: what a row `max`
/// cells wide shows of it.
pub fn cut(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (at, c) in s.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            return &s[..at];
        }
        used += w;
    }
    s
}

/// Cut `s` to at most `max` cells, ending in `…` when anything was cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    format!("{}…", cut(s, max - 1))
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
        assert_eq!(gwei(0.0), "0");
        assert_eq!(gwei(0.41), "0.410");
        assert_eq!(gwei(0.002), "0.002");
        assert_eq!(gwei(7e-9), "<0.001");
        assert_eq!(gwei(4.23), "4.23");
        assert_eq!(gwei(31.4), "31.4");
        assert_eq!(gwei(131.4), "131");
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
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("infura", 10), "infura");
        assert_eq!(truncate("publicnode-2", 8), "publicn…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("日本語", 4), "日…");
        assert_eq!(cut("日本語", 5), "日本");
        assert_eq!(cut("abc", 9), "abc");
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
}
