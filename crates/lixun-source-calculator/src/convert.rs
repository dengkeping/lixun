//! Static conversion pass: unit conversions ("5km in mi"), base
//! conversions ("0xff", "255 in hex"), and percent-of ("15% of 80").
//!
//! Runs before the meval fallback in [`crate::detect::detect`] because
//! these inputs contain identifiers `looks_like_math` rejects. The
//! grammars here are deliberately exact — anything that does not match a
//! full pattern returns `None`, so plain prose ("made in china", "cash
//! in hand") never produces a hit. A bare quantity with no target unit
//! ("5 in" — five inches) is not a conversion and is rejected too. This
//! extends the conservative philosophy of `looks_like_math`: false
//! positives are worse than false negatives.

use crate::detect::{format_result, MAX_INPUT_LEN};
use lixun_core::Calculation;

/// Significant digits for converted quantities ("3.106856 mi").
const SIGNIFICANT_DIGITS: i32 = 7;

/// Entry point: try each conversion grammar against the trimmed input.
/// Returns `None` unless one grammar matches completely.
pub(crate) fn convert(input: &str) -> Option<Calculation> {
    let expr = input.trim();
    if expr.is_empty() || expr.len() > MAX_INPUT_LEN {
        return None;
    }
    percent_of(expr)
        .or_else(|| base_literal(expr))
        .or_else(|| base_conversion(expr))
        .or_else(|| unit_conversion(expr))
}

// ---------------------------------------------------------------------------
// Unit conversion: `<number> <unit> (in|to|as) <unit>`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dimension {
    Length,
    Mass,
    Temperature,
    Data,
    Time,
}

/// A unit and its affine mapping into the dimension's base unit:
/// `value_in_base = value * scale + offset`. Only temperature uses a
/// non-zero offset (base kelvin); every other dimension is a pure factor
/// (bases: metre, kilogram, byte, second).
struct UnitDef {
    /// Accepted spellings, all lowercase (lookup lowercases the token).
    names: &'static [&'static str],
    /// Canonical suffix used in the result string.
    display: &'static str,
    dimension: Dimension,
    scale: f64,
    offset: f64,
}

const fn linear(
    names: &'static [&'static str],
    display: &'static str,
    dimension: Dimension,
    scale: f64,
) -> UnitDef {
    UnitDef {
        names,
        display,
        dimension,
        scale,
        offset: 0.0,
    }
}

static UNITS: &[UnitDef] = &[
    // Length (base: metre).
    linear(&["mm"], "mm", Dimension::Length, 1e-3),
    linear(&["cm"], "cm", Dimension::Length, 1e-2),
    linear(&["m", "meter", "meters", "metre", "metres"], "m", Dimension::Length, 1.0),
    linear(&["km"], "km", Dimension::Length, 1e3),
    linear(&["in", "inch", "inches"], "in", Dimension::Length, 0.0254),
    linear(&["ft", "feet", "foot"], "ft", Dimension::Length, 0.3048),
    linear(&["yd", "yard", "yards"], "yd", Dimension::Length, 0.9144),
    linear(&["mi", "mile", "miles"], "mi", Dimension::Length, 1609.344),
    // Mass (base: kilogram).
    linear(&["mg"], "mg", Dimension::Mass, 1e-6),
    linear(&["g", "gram", "grams"], "g", Dimension::Mass, 1e-3),
    linear(&["kg"], "kg", Dimension::Mass, 1.0),
    linear(&["t", "tonne", "tonnes"], "t", Dimension::Mass, 1e3),
    linear(&["oz", "ounce", "ounces"], "oz", Dimension::Mass, 0.028349523125),
    linear(&["lb", "lbs", "pound", "pounds"], "lb", Dimension::Mass, 0.45359237),
    linear(&["st", "stone", "stones"], "st", Dimension::Mass, 6.35029318),
    // Temperature (base: kelvin; affine, not just a factor).
    UnitDef {
        names: &["c", "°c", "celsius"],
        display: "°C",
        dimension: Dimension::Temperature,
        scale: 1.0,
        offset: 273.15,
    },
    UnitDef {
        names: &["f", "°f", "fahrenheit"],
        display: "°F",
        dimension: Dimension::Temperature,
        scale: 5.0 / 9.0,
        offset: 459.67 * 5.0 / 9.0,
    },
    UnitDef {
        names: &["k", "kelvin"],
        display: "K",
        dimension: Dimension::Temperature,
        scale: 1.0,
        offset: 0.0,
    },
    // Data (base: byte). Decimal prefixes are powers of 1000, binary
    // ("i" infix) prefixes are powers of 1024 — kB = 1000 B, KiB = 1024 B.
    linear(&["bit", "bits"], "bit", Dimension::Data, 0.125),
    linear(&["byte", "bytes"], "B", Dimension::Data, 1.0),
    linear(&["kb", "kilobyte", "kilobytes"], "kB", Dimension::Data, 1e3),
    linear(&["mb", "megabyte", "megabytes"], "MB", Dimension::Data, 1e6),
    linear(&["gb", "gigabyte", "gigabytes"], "GB", Dimension::Data, 1e9),
    linear(&["tb", "terabyte", "terabytes"], "TB", Dimension::Data, 1e12),
    linear(&["kib", "kibibyte", "kibibytes"], "KiB", Dimension::Data, 1024.0),
    linear(&["mib", "mebibyte", "mebibytes"], "MiB", Dimension::Data, 1024.0 * 1024.0),
    linear(&["gib", "gibibyte", "gibibytes"], "GiB", Dimension::Data, 1024.0 * 1024.0 * 1024.0),
    linear(
        &["tib", "tebibyte", "tebibytes"],
        "TiB",
        Dimension::Data,
        1024.0 * 1024.0 * 1024.0 * 1024.0,
    ),
    // Time (base: second).
    linear(&["ms", "millisecond", "milliseconds"], "ms", Dimension::Time, 1e-3),
    linear(&["s", "sec", "secs", "second", "seconds"], "s", Dimension::Time, 1.0),
    linear(&["min", "mins", "minute", "minutes"], "min", Dimension::Time, 60.0),
    linear(&["h", "hr", "hrs", "hour", "hours"], "h", Dimension::Time, 3600.0),
    linear(&["d", "day", "days"], "d", Dimension::Time, 86400.0),
    linear(&["week", "weeks", "wk"], "wk", Dimension::Time, 604800.0),
];

/// Resolve a unit token.
///
/// Case policy: unit spellings match case-insensitively ("KM" == "km",
/// "Kg" == "kg") because the table contains no pair distinguished only
/// by case — with one exception: the bare data symbols "b" (bit) and
/// "B" (byte), where case is the only signal, so they match exactly.
/// Prefixed byte symbols (kB/MB/GB/TB, KiB/MiB/GiB/TiB) also match
/// case-insensitively: the table carries no prefixed bit units
/// (Mb-as-megabit is not supported), so "mb" can only mean megabyte
/// here, and decimal vs binary is decided by the "i" infix (kB = 1000 B,
/// KiB = 1024 B), never by case.
fn lookup_unit(token: &str) -> Option<&'static UnitDef> {
    let lower;
    let name = match token {
        "b" => "bit",
        "B" => "byte",
        _ => {
            lower = token.to_lowercase();
            lower.as_str()
        }
    };
    UNITS.iter().find(|unit| unit.names.contains(&name))
}

fn is_conversion_keyword(token: &str) -> bool {
    token.eq_ignore_ascii_case("in")
        || token.eq_ignore_ascii_case("to")
        || token.eq_ignore_ascii_case("as")
}

/// Split a leading number (optional minus, digits, at most one dot) off
/// the input. Whitespace between number and the remainder is optional,
/// so "5km" and "5 km" both parse. No exponent notation.
fn split_leading_number(s: &str) -> Option<(f64, &str)> {
    let bytes = s.as_bytes();
    let mut end = 0;
    if bytes.first() == Some(&b'-') {
        end += 1;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => {
                saw_digit = true;
                end += 1;
            }
            b'.' if !saw_dot => {
                saw_dot = true;
                end += 1;
            }
            _ => break,
        }
    }
    if !saw_digit {
        return None;
    }
    let value: f64 = s[..end].parse().ok()?;
    Some((value, &s[end..]))
}

/// Parse a string that must be exactly one number, nothing else.
fn parse_full_number(s: &str) -> Option<f64> {
    let (value, rest) = split_leading_number(s)?;
    rest.is_empty().then_some(value)
}

fn unit_conversion(expr: &str) -> Option<Calculation> {
    let (value, rest) = split_leading_number(expr)?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    // Exactly `<unit> <keyword> <unit>` after the number. A lone unit
    // ("5 in") is a bare quantity, not a conversion; extra words are
    // prose. Both must miss.
    let &[src_token, keyword, dst_token] = tokens.as_slice() else {
        return None;
    };
    if !is_conversion_keyword(keyword) {
        return None;
    }
    let src = lookup_unit(src_token)?;
    let dst = lookup_unit(dst_token)?;
    if src.dimension != dst.dimension {
        return None;
    }
    let base = value * src.scale + src.offset;
    let converted = (base - dst.offset) / dst.scale;
    Some(Calculation {
        expr: expr.to_string(),
        result: format!("{} {}", format_quantity(converted), dst.display),
    })
}

/// Format a converted quantity: integers bare, otherwise rounded to
/// [`SIGNIFICANT_DIGITS`] significant digits with trailing zeros
/// trimmed. Mirrors the style of `detect::format_result`.
fn format_quantity(x: f64) -> String {
    if !x.is_finite() {
        return "Error".to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    if x.fract() == 0.0 && x.abs() < 1e15 {
        return format!("{x:.0}");
    }
    let exponent = x.abs().log10().floor() as i32;
    let precision = (SIGNIFICANT_DIGITS - 1 - exponent).clamp(0, 12) as usize;
    let rendered = format!("{x:.precision$}");
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

// ---------------------------------------------------------------------------
// Base conversion: `0xff`, `0b1010`, `<int> (in|to|as) (hex|bin|dec)`
// ---------------------------------------------------------------------------

/// Parse an unsigned integer literal: plain decimal digits, `0x` hex, or
/// `0b` binary (prefix and digits case-insensitive). Negative values are
/// rejected — base output for negatives is not supported.
fn parse_int_literal(token: &str) -> Option<u64> {
    let lower = token.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        if hex.is_empty() {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = lower.strip_prefix("0b") {
        if bin.is_empty() {
            return None;
        }
        u64::from_str_radix(bin, 2).ok()
    } else if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) {
        token.parse::<u64>().ok()
    } else {
        None
    }
}

/// A standalone `0x`/`0b` literal evaluates to its decimal value. Bare
/// decimal numbers do not fire here — "255" alone is not a conversion.
fn base_literal(expr: &str) -> Option<Calculation> {
    if expr.contains(char::is_whitespace) {
        return None;
    }
    let lower = expr.to_ascii_lowercase();
    if !lower.starts_with("0x") && !lower.starts_with("0b") {
        return None;
    }
    let value = parse_int_literal(expr)?;
    Some(Calculation {
        expr: expr.to_string(),
        result: value.to_string(),
    })
}

fn base_conversion(expr: &str) -> Option<Calculation> {
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    let &[literal, keyword, base] = tokens.as_slice() else {
        return None;
    };
    if !is_conversion_keyword(keyword) {
        return None;
    }
    let value = parse_int_literal(literal)?;
    let result = match base.to_ascii_lowercase().as_str() {
        "hex" | "hexadecimal" => format!("0x{value:x}"),
        "bin" | "binary" => format!("0b{value:b}"),
        "dec" | "decimal" => value.to_string(),
        _ => return None,
    };
    Some(Calculation {
        expr: expr.to_string(),
        result,
    })
}

// ---------------------------------------------------------------------------
// Percent-of: `<number>% of <number>`
// ---------------------------------------------------------------------------

fn percent_of(expr: &str) -> Option<Calculation> {
    let (percent_part, rest) = expr.split_once('%')?;
    let percent = parse_full_number(percent_part.trim_end())?;
    let rest = rest.trim_start();
    // Keyword "of" (case-insensitive) followed by whitespace and the
    // amount, nothing after.
    if !rest.get(..2).is_some_and(|kw| kw.eq_ignore_ascii_case("of")) {
        return None;
    }
    let after_of = &rest[2..];
    if !after_of.starts_with(char::is_whitespace) {
        return None;
    }
    let amount = parse_full_number(after_of.trim())?;
    let value = percent / 100.0 * amount;
    Some(Calculation {
        expr: expr.to_string(),
        result: format_result(value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_of(input: &str) -> Option<String> {
        convert(input).map(|calc| calc.result)
    }

    // -- length --------------------------------------------------------

    #[test]
    fn length_km_to_mi() {
        assert_eq!(result_of("5km in mi").as_deref(), Some("3.106856 mi"));
    }

    #[test]
    fn length_whitespace_between_number_and_unit_is_optional() {
        assert_eq!(result_of("5 km in mi"), result_of("5km in mi"));
    }

    #[test]
    fn length_inches_to_cm() {
        assert_eq!(result_of("12 in to cm").as_deref(), Some("30.48 cm"));
    }

    #[test]
    fn length_mi_to_km() {
        assert_eq!(result_of("1 mi in km").as_deref(), Some("1.609344 km"));
    }

    #[test]
    fn length_mm_to_cm() {
        assert_eq!(result_of("100 mm in cm").as_deref(), Some("10 cm"));
    }

    #[test]
    fn length_yd_to_ft() {
        assert_eq!(result_of("2 yd in ft").as_deref(), Some("6 ft"));
    }

    #[test]
    fn length_word_units() {
        assert_eq!(result_of("3 feet in inches").as_deref(), Some("36 in"));
    }

    #[test]
    fn length_units_match_case_insensitively() {
        assert_eq!(result_of("5KM in MI").as_deref(), Some("3.106856 mi"));
    }

    // -- mass ----------------------------------------------------------

    #[test]
    fn mass_kg_to_lb() {
        assert_eq!(result_of("1 kg in lb").as_deref(), Some("2.204623 lb"));
    }

    #[test]
    fn mass_oz_to_lb() {
        assert_eq!(result_of("16 oz in lb").as_deref(), Some("1 lb"));
    }

    #[test]
    fn mass_stone_to_kg() {
        assert_eq!(result_of("1 st in kg").as_deref(), Some("6.350293 kg"));
    }

    #[test]
    fn mass_mg_to_g() {
        assert_eq!(result_of("1000 mg in g").as_deref(), Some("1 g"));
    }

    #[test]
    fn mass_tonne_to_kg() {
        assert_eq!(result_of("2 t in kg").as_deref(), Some("2000 kg"));
    }

    // -- temperature (affine) ------------------------------------------

    #[test]
    fn temperature_f_to_c_is_affine() {
        // 72 °F = (72 - 32) * 5/9 ≈ 22.22 °C; a factor-only conversion
        // would give a wildly different number.
        let result = result_of("72F in C").expect("conversion fires");
        let value: f64 = result
            .strip_suffix(" °C")
            .expect("celsius suffix")
            .parse()
            .expect("numeric");
        assert!((value - 22.2222).abs() < 1e-3, "got {result}");
    }

    #[test]
    fn temperature_c_to_f_freezing_point() {
        assert_eq!(result_of("0C in F").as_deref(), Some("32 °F"));
    }

    #[test]
    fn temperature_c_to_f_boiling_point() {
        assert_eq!(result_of("100 c in f").as_deref(), Some("212 °F"));
    }

    #[test]
    fn temperature_absolute_zero() {
        assert_eq!(result_of("0 K in C").as_deref(), Some("-273.15 °C"));
    }

    #[test]
    fn temperature_negative_forty_crossover() {
        assert_eq!(result_of("-40C in F").as_deref(), Some("-40 °F"));
    }

    #[test]
    fn temperature_degree_sign_spellings() {
        assert_eq!(result_of("0 °C in °F").as_deref(), Some("32 °F"));
    }

    #[test]
    fn temperature_word_spellings() {
        assert_eq!(result_of("0 celsius in fahrenheit").as_deref(), Some("32 °F"));
    }

    // -- data sizes ----------------------------------------------------

    #[test]
    fn data_decimal_kilobyte_is_1000_bytes() {
        assert_eq!(result_of("1 kB in B").as_deref(), Some("1000 B"));
    }

    #[test]
    fn data_binary_kibibyte_is_1024_bytes() {
        assert_eq!(result_of("1 KiB in B").as_deref(), Some("1024 B"));
    }

    #[test]
    fn data_mib_vs_mb_distinction() {
        assert_eq!(result_of("1 MiB in MB").as_deref(), Some("1.048576 MB"));
    }

    #[test]
    fn data_gib_to_mb() {
        assert_eq!(result_of("1 GiB in MB").as_deref(), Some("1073.742 MB"));
    }

    #[test]
    fn data_tb_to_gb() {
        assert_eq!(result_of("2 TB in GB").as_deref(), Some("2000 GB"));
    }

    #[test]
    fn data_bits_to_bytes() {
        assert_eq!(result_of("8 bit in B").as_deref(), Some("1 B"));
    }

    #[test]
    fn data_bare_b_is_bit_and_bare_upper_b_is_byte() {
        // Bare "b"/"B" is the one case-sensitive pair in the table.
        assert_eq!(result_of("1 B in bit").as_deref(), Some("8 bit"));
        assert_eq!(result_of("16 b in B").as_deref(), Some("2 B"));
    }

    #[test]
    fn data_prefixed_symbols_are_case_insensitive() {
        // "mb" has no megabit reading in this table, so any casing of a
        // prefixed symbol resolves to the byte unit.
        assert_eq!(result_of("1 mb in kb").as_deref(), Some("1000 kB"));
        assert_eq!(result_of("1 kib in b").as_deref(), Some("8192 bit"));
    }

    // -- time ----------------------------------------------------------

    #[test]
    fn time_minutes_to_hours() {
        assert_eq!(result_of("90 min in h").as_deref(), Some("1.5 h"));
    }

    #[test]
    fn time_hours_to_seconds() {
        assert_eq!(result_of("1 h in s").as_deref(), Some("3600 s"));
    }

    #[test]
    fn time_weeks_to_days() {
        assert_eq!(result_of("2 weeks in d").as_deref(), Some("14 d"));
    }

    #[test]
    fn time_ms_to_seconds() {
        assert_eq!(result_of("1500 ms in s").as_deref(), Some("1.5 s"));
    }

    // -- hex / binary --------------------------------------------------

    #[test]
    fn hex_literal_to_decimal() {
        assert_eq!(result_of("0xff").as_deref(), Some("255"));
    }

    #[test]
    fn binary_literal_to_decimal() {
        assert_eq!(result_of("0b1010").as_deref(), Some("10"));
    }

    #[test]
    fn decimal_to_hex() {
        assert_eq!(result_of("255 in hex").as_deref(), Some("0xff"));
    }

    #[test]
    fn decimal_to_binary() {
        assert_eq!(result_of("10 in bin").as_deref(), Some("0b1010"));
        assert_eq!(result_of("10 in binary").as_deref(), Some("0b1010"));
    }

    #[test]
    fn hex_to_binary_and_back() {
        assert_eq!(result_of("0xff in bin").as_deref(), Some("0b11111111"));
        assert_eq!(result_of("0b11111111 in hex").as_deref(), Some("0xff"));
    }

    #[test]
    fn hex_to_decimal_keyword() {
        assert_eq!(result_of("0xff in dec").as_deref(), Some("255"));
        assert_eq!(result_of("0b1010 in decimal").as_deref(), Some("10"));
    }

    #[test]
    fn base_conversion_rejects_negative_values() {
        assert_eq!(convert("-5 in hex"), None);
        assert_eq!(convert("-0b10 in dec"), None);
    }

    #[test]
    fn base_conversion_rejects_malformed_literals() {
        assert_eq!(convert("0x"), None);
        assert_eq!(convert("0xzz"), None);
        assert_eq!(convert("0x1g in dec"), None);
    }

    // -- percent-of ----------------------------------------------------

    #[test]
    fn percent_of_integer() {
        assert_eq!(result_of("15% of 80").as_deref(), Some("12"));
    }

    #[test]
    fn percent_of_decimal_percentage() {
        assert_eq!(result_of("12.5% of 80").as_deref(), Some("10"));
    }

    #[test]
    fn percent_of_over_hundred() {
        assert_eq!(result_of("150% of 4").as_deref(), Some("6"));
    }

    #[test]
    fn percent_of_allows_space_before_percent_sign() {
        assert_eq!(result_of("15 % of 80").as_deref(), Some("12"));
    }

    #[test]
    fn percent_of_rejects_trailing_words() {
        assert_eq!(convert("15% of 80 please"), None);
        assert_eq!(convert("15% off 80"), None);
        assert_eq!(convert("15% of"), None);
    }

    // -- prose and ambiguity rejections --------------------------------

    #[test]
    fn rejects_plain_words() {
        assert_eq!(convert("firefox"), None);
        assert_eq!(convert("made in china"), None);
        assert_eq!(convert("cash in hand"), None);
    }

    #[test]
    fn rejects_bare_quantity_without_target() {
        // "5 in" is five inches, not a conversion request.
        assert_eq!(convert("5 in"), None);
        assert_eq!(convert("5km"), None);
    }

    #[test]
    fn rejects_keyword_only_fragments() {
        assert_eq!(convert("in to"), None);
        assert_eq!(convert("in in in"), None);
    }

    #[test]
    fn rejects_conversion_without_number() {
        assert_eq!(convert("km in mi"), None);
    }

    #[test]
    fn rejects_cross_dimension_conversion() {
        assert_eq!(convert("5 km in kg"), None);
        assert_eq!(convert("1 h in MB"), None);
    }

    #[test]
    fn rejects_unknown_units() {
        assert_eq!(convert("5 xy in km"), None);
        assert_eq!(convert("5 km in zz"), None);
    }

    #[test]
    fn rejects_extra_tokens() {
        assert_eq!(convert("5 km in mi now"), None);
        assert_eq!(convert("about 5 km in mi"), None);
    }

    #[test]
    fn rejects_empty_and_oversized_input() {
        assert_eq!(convert(""), None);
        assert_eq!(convert("   "), None);
        let long = format!("5km in mi{}", " ".repeat(300));
        // Oversized after trim is fine (trailing spaces trim away) —
        // build one that stays oversized.
        assert!(convert(&long).is_some());
        let oversized = format!("5{}km in mi", "0".repeat(300));
        assert_eq!(convert(&oversized), None);
    }

    #[test]
    fn negative_and_decimal_numbers_parse() {
        assert_eq!(result_of("-2.5 km in m").as_deref(), Some("-2500 m"));
        assert_eq!(result_of("0.5 h in min").as_deref(), Some("30 min"));
    }

    #[test]
    fn identity_conversion_is_allowed() {
        assert_eq!(result_of("5 km in km").as_deref(), Some("5 km"));
    }

    #[test]
    fn keyword_variants_and_case() {
        assert_eq!(result_of("5km to mi").as_deref(), Some("3.106856 mi"));
        assert_eq!(result_of("5km as mi").as_deref(), Some("3.106856 mi"));
        assert_eq!(result_of("5km IN mi").as_deref(), Some("3.106856 mi"));
    }

    #[test]
    fn expr_field_carries_trimmed_input() {
        let calc = convert("  5km in mi  ").expect("conversion fires");
        assert_eq!(calc.expr, "5km in mi");
    }
}
