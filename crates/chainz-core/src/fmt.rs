//! Small display helpers shared by every chainz surface (CLI, gateway,
//! dashboard), so a number, an uptime or an address reads the same
//! everywhere.

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

/// `0x7E5F4552…` → `0x7E5F…5Bdf`. Anything that is not a 42-character
/// address comes back unchanged.
pub fn short_address(address: &str) -> String {
    if address.len() == 42 && address.is_ascii() {
        format!("{}…{}", &address[..6], &address[38..])
    } else {
        address.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn short_addresses() {
        let address = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
        assert_eq!(short_address(address), "0xf39F…2266");
        assert_eq!(short_address("0x1234"), "0x1234");
    }
}
