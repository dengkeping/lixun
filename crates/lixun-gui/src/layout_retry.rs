/// Physical US/Russian keyboard positions. Only propose a switch for plain
/// text in a single alphabet. The user activates the replacement row.
pub(crate) fn alternative(query: &str) -> Option<String> {
    const US: &str = "`qwertyuiop[]asdfghjkl;'zxcvbnm,./";
    const RU: &str = "ёйцукенгшщзхъфывапролджэячсмитьбю.";
    let query = query.trim();
    if query.is_empty() || query.contains(['/', '\\']) || lixun_core::query_uses_operators(query) {
        return None;
    }
    let latin = query.chars().any(|c| c.is_ascii_alphabetic());
    let russian = query
        .chars()
        .any(|c| RU.contains(c.to_lowercase().next().unwrap()) && c.is_alphabetic());
    if latin == russian {
        return None;
    }
    let (from, to) = if latin { (US, RU) } else { (RU, US) };
    let mut result = String::new();
    for c in query.chars() {
        if c == ' ' || c.is_ascii_digit() {
            result.push(c);
            continue;
        }
        let lower = c.to_lowercase().next()?;
        let index = from.chars().position(|k| k == lower)?;
        let mapped = to.chars().nth(index)?;
        if c.is_uppercase() {
            result.extend(mapped.to_uppercase());
        } else {
            result.push(mapped);
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swaps_both_directions_and_preserves_operators() {
        assert_eq!(alternative("Ghbdtn").as_deref(), Some("Привет"));
        assert_eq!(alternative("руддщ").as_deref(), Some("hello"));
        for q in ["title:руддщ", "-draft", "aя", "!ls", "123", "file.pdf/path"] {
            assert_eq!(alternative(q), None);
        }
    }
}
