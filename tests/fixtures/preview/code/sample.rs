//! Sample Rust file for code-preview QA.
//! NEEDLE-A appears in this comment for search tests.

use std::collections::HashMap;

pub fn fizzbuzz(n: u32) -> Vec<String> {
    (1..=n)
        .map(|i| match (i % 3, i % 5) {
            (0, 0) => "fizzbuzz".into(),
            (0, _) => "fizz".into(),
            (_, 0) => "buzz".into(),
            _ => i.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_15() {
        let v = fizzbuzz(15);
        assert_eq!(v.last().unwrap(), "fizzbuzz");
    }
}

#[allow(dead_code)]
fn _padding() -> HashMap<&'static str, u32> {
    HashMap::new()
}
