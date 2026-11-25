//! Small text helpers.

/// Collapses runs of whitespace into single spaces and trims the ends.
pub fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squeezes() {
        assert_eq!(squeeze("  a   b \n c "), "a b c");
    }
}
