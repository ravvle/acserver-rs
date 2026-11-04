//! Value parsing for INI settings.

/// A decimal integer with an optional sign, as vanilla's `strconv.Atoi`.
pub fn parse_int(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// Decimal digits only: a sign, space or prefix is refused, and so is a
/// value out of range.
pub fn parse_digits<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.starts_with(['+', '-']) {
        return None; // FromStr accepts a sign
    }
    s.parse().ok()
}

/// A Steam ID: a u64 in decimal digits only.
pub fn parse_steam_id(s: &str) -> Option<u64> {
    parse_digits(s)
}

/// A decimal float, `inf` or `nan`.
pub fn parse_float(s: &str) -> Option<f64> {
    s.parse::<f64>().ok()
}

/// Vanilla reads a flag as an integer, non-zero meaning on; true/false,
/// yes/no and on/off are accepted too, in any case.
pub fn parse_bool(s: &str) -> Option<bool> {
    if let Some(n) = parse_int(s) {
        return Some(n != 0);
    }
    match s.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The number in a `CAR_<n>` section name.
pub fn scan_car_index(name: &str) -> Option<i64> {
    parse_digits(name.strip_prefix("CAR_")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_are_decimal() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-7"), Some(-7));
        assert_eq!(parse_int("+7"), Some(7));
        assert_eq!(parse_int("010"), Some(10));
        assert_eq!(parse_int("0x1F"), None);
        assert_eq!(parse_int("5.0"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn bools() {
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("2"), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("True"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn car_index() {
        assert_eq!(scan_car_index("CAR_10"), Some(10));
        assert_eq!(scan_car_index("CAR_1a"), None);
        assert_eq!(scan_car_index("CAR_"), None);
    }
}
