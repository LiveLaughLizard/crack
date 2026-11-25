/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

/// The sum of a list of numbers.
pub fn sum(values: &[i32]) -> i32 {
    values.iter().fold(0, |total, v| add(total, *v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn sums() {
        assert_eq!(sum(&[1, 2, 3, 4]), 10);
    }
}
