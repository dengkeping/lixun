//! Inline calculator: detect math-like queries and evaluate them.

use lixun_core::Calculation;

const MAX_INPUT_LEN: usize = 256;
const FUNCTIONS: &[&str] = &[
    "sqrt", "sin", "cos", "tan", "asin", "acos", "atan", "ln", "log", "exp", "abs", "floor", "ceil",
];
const CONSTANTS: &[&str] = &["pi", "e"];

/// Heuristic: does the input look like a math expression?
/// Criteria (all must hold after trim):
/// - non-empty,
/// - length ≤ 256 chars,
/// - contains at least one math operator (+, -, *, /, ^, %) OR one known
///   function name (sqrt, sin, cos, tan, asin, acos, atan, ln, log, exp,
///   abs, floor, ceil) OR one of the constants (pi, e) as a whole word,
/// - every non-whitespace character is either a digit, dot, operator
///   (+, -, *, /, ^, %, (, ), comma) OR belongs to an alphabetic run that
///   is a recognized function name / constant.
///
/// Conservative: false positives are worse than false negatives.
pub fn looks_like_math(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_INPUT_LEN || trimmed.contains('!') {
        return false;
    }

    let mut saw_math_signal = false;
    let chars: Vec<char> = trimmed.chars().collect();
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        if ch.is_whitespace() {
            index += 1;
            continue;
        }

        if ch.is_ascii_digit()
            || matches!(
                ch,
                '.' | '+' | '-' | '*' | '/' | '^' | '%' | '(' | ')' | ','
            )
        {
            if matches!(ch, '+' | '-' | '*' | '/' | '^' | '%') {
                saw_math_signal = true;
            }
            index += 1;
            continue;
        }

        if ch.is_ascii_alphabetic() {
            let start = index;
            index += 1;
            while index < chars.len() && chars[index].is_ascii_alphabetic() {
                index += 1;
            }

            let ident: String = chars[start..index].iter().collect();
            if FUNCTIONS.contains(&ident.as_str()) || CONSTANTS.contains(&ident.as_str()) {
                saw_math_signal = true;
                continue;
            }

            return false;
        }

        return false;
    }

    saw_math_signal
}

/// Try to evaluate `input` as a math expression via meval.
/// Returns `Some(Calculation)` iff `looks_like_math(input)` AND evaluation
/// produced a finite result OR a well-defined special value (NaN/Inf map
/// to an error-style result).
pub fn detect(input: &str) -> Option<Calculation> {
    let expr = input.trim();
    if !looks_like_math(expr) {
        return None;
    }

    let value = meval::eval_str(expr).ok()?;

    Some(Calculation {
        expr: expr.to_string(),
        result: format_result(value),
    })
}

/// Format an f64 result as a short, trimmed human string.
/// - Integers without fractional part: `"42"`, `"-7"`.
/// - Otherwise up to 10 significant digits, trailing zeros stripped.
/// - NaN / Inf → "Error".
fn format_result(x: f64) -> String {
    if !x.is_finite() {
        return "Error".to_string();
    }

    if x.fract() == 0.0 {
        return format!("{x:.0}");
    }

    let rendered = format!("{x:.10}");
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_math_accepts_basic_arithmetic() {
        assert!(looks_like_math("2+2"));
    }

    #[test]
    fn looks_like_math_rejects_plain_word() {
        assert!(!looks_like_math("firefox"));
    }

    #[test]
    fn looks_like_math_rejects_empty_input() {
        assert!(!looks_like_math(""));
    }

    #[test]
    fn looks_like_math_rejects_sentence() {
        assert!(!looks_like_math("hello world"));
    }

    #[test]
    fn looks_like_math_accepts_function_call() {
        assert!(looks_like_math("sqrt(16)"));
    }

    #[test]
    fn looks_like_math_accepts_constant() {
        assert!(looks_like_math("pi"));
    }

    #[test]
    fn looks_like_math_rejects_unknown_identifiers() {
        assert!(!looks_like_math("abc+def"));
    }

    #[test]
    fn looks_like_math_rejects_factorial() {
        assert!(!looks_like_math("5!"));
    }

    #[test]
    fn looks_like_math_rejects_long_input() {
        let input = format!("2+{}", "1".repeat(256));
        assert!(!looks_like_math(&input));
    }

    #[test]
    fn detect_evaluates_basic_arithmetic() {
        assert_eq!(
            detect("2+2"),
            Some(Calculation {
                expr: "2+2".to_string(),
                result: "4".to_string(),
            })
        );
    }

    #[test]
    fn detect_trims_expression() {
        assert_eq!(
            detect(" 5*(3+2) "),
            Some(Calculation {
                expr: "5*(3+2)".to_string(),
                result: "25".to_string(),
            })
        );
    }

    #[test]
    fn detect_maps_infinity_to_error() {
        assert_eq!(
            detect("1/0"),
            Some(Calculation {
                expr: "1/0".to_string(),
                result: "Error".to_string(),
            })
        );
    }

    #[test]
    fn detect_evaluates_sqrt() {
        assert_eq!(
            detect("sqrt(16)"),
            Some(Calculation {
                expr: "sqrt(16)".to_string(),
                result: "4".to_string(),
            })
        );
    }

    #[test]
    fn detect_evaluates_function_and_constant() {
        // sqrt(16) + pi = 7.1415926535...; format_result uses 10 sig figs,
        // so accept the actually-rendered value.
        let got = detect("sqrt(16)+pi").expect("detect returns Some");
        assert_eq!(got.expr, "sqrt(16)+pi");
        // Parse the rendered result back and compare numerically with tolerance.
        let rendered: f64 = got.result.parse().expect("numeric result");
        assert!(
            (rendered - 7.141_592_653_6).abs() < 1e-6,
            "unexpected result: {}",
            got.result
        );
    }

    #[test]
    fn detect_evaluates_trig_function() {
        assert_eq!(
            detect("sin(pi/2)"),
            Some(Calculation {
                expr: "sin(pi/2)".to_string(),
                result: "1".to_string(),
            })
        );
    }

    #[test]
    fn detect_evaluates_power_operator() {
        assert_eq!(
            detect("2^10"),
            Some(Calculation {
                expr: "2^10".to_string(),
                result: "1024".to_string(),
            })
        );
    }

    #[test]
    fn detect_rejects_empty_input() {
        assert_eq!(detect(""), None);
    }

    #[test]
    fn detect_rejects_plain_word() {
        assert_eq!(detect("firefox"), None);
    }

    #[test]
    fn detect_rejects_non_math_sentence() {
        assert_eq!(detect("not math at all"), None);
    }

    #[test]
    fn detect_rejects_syntax_error() {
        assert_eq!(detect("2++"), None);
    }

    #[test]
    fn detect_rejects_unknown_identifier() {
        assert_eq!(detect("abc+def"), None);
    }

    #[test]
    fn detect_formats_fractional_results() {
        assert_eq!(
            detect("1/8"),
            Some(Calculation {
                expr: "1/8".to_string(),
                result: "0.125".to_string(),
            })
        );
    }

    #[test]
    fn detect_supports_floor_function() {
        assert_eq!(
            detect("floor(3.9)"),
            Some(Calculation {
                expr: "floor(3.9)".to_string(),
                result: "3".to_string(),
            })
        );
    }

    #[test]
    fn detect_supports_negative_numbers() {
        assert_eq!(
            detect("-7+2"),
            Some(Calculation {
                expr: "-7+2".to_string(),
                result: "-5".to_string(),
            })
        );
    }

    #[test]
    fn detect_returns_error_for_nan() {
        assert_eq!(
            detect("sqrt(-1)"),
            Some(Calculation {
                expr: "sqrt(-1)".to_string(),
                result: "Error".to_string(),
            })
        );
    }
}
