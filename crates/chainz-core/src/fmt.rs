//! Small display helpers shared by the CLI, `serve` and the dashboard, so
//! a number, an uptime or an address reads the same everywhere.

/// `23481220` → `23,481,220`.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An uptime: `42s`, `12m05s`, `2h03m`, `3d04h`.
pub fn uptime(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m{:02}s", secs / 60, secs % 60),
        3_600..=86_399 => format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d{:02}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// A coarse age (`top`, `ls CHAIN`): `4s`, `3m`, `2h`, `5d`.
pub fn age(secs: u64) -> String {
    match secs {
        0..=99 => format!("{secs}s"),
        100..=5_999 => format!("{}m", secs / 60),
        6_000..=172_799 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `0x7E5F4552…` → `0x7E5F…5Bdf` (checksummed).
pub fn short_address(address: &crate::Address) -> String {
    let address = address.to_checksum();
    format!("{}…{}", &address[..6], &address[38..])
}

/// `n` and the noun, plural unless `n` is 1: `1 chain`, `3 chains`.
pub fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// Where something applies: up to three names (`eth, base`), else a
/// [`count`] of `noun` (`17 chains`).
pub fn names_or_count<S: AsRef<str>>(names: &[S], noun: &str) -> String {
    match names.len() {
        0..=3 => names
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", "),
        n => count(n, noun),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_few_names_or_a_count() {
        assert_eq!(count(1, "chain"), "1 chain");
        assert_eq!(count(0, "chain"), "0 chains");
        assert_eq!(names_or_count(&["eth", "base"], "chain"), "eth, base");
        let many: Vec<String> = (0..4).map(|i| format!("c{i}")).collect();
        assert_eq!(names_or_count(&many, "chain"), "4 chains");
    }

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(38_112_904), "38,112,904");
        assert_eq!(group(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn uptimes() {
        assert_eq!(uptime(0), "0s");
        assert_eq!(uptime(42), "42s");
        assert_eq!(uptime(125), "2m05s");
        assert_eq!(uptime(725), "12m05s");
        assert_eq!(uptime(7_380), "2h03m");
        assert_eq!(uptime(273_600), "3d04h");
    }

    #[test]
    fn ages() {
        assert_eq!(age(4), "4s");
        assert_eq!(age(99), "99s");
        assert_eq!(age(100), "1m");
        assert_eq!(age(7_200), "2h");
        assert_eq!(age(400_000), "4d");
    }

    #[test]
    fn short_addresses() {
        let address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .parse()
            .unwrap();
        assert_eq!(short_address(&address), "0xf39F…2266");
    }
}
