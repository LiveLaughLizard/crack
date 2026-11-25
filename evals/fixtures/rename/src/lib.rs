pub mod cart;
pub mod report;

/// The total price of `(price, quantity)` lines, in cents.
pub fn compute_total(lines: &[(u64, u64)]) -> u64 {
    lines.iter().map(|(price, quantity)| price * quantity).sum()
}
