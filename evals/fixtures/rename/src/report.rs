/// A one-line summary, e.g. "3 items, $12.50".
pub fn summary(lines: &[(u64, u64)]) -> String {
    let items: u64 = lines.iter().map(|(_, quantity)| quantity).sum();
    let cents = crate::compute_total(lines);
    format!("{items} items, ${}.{:02}", cents / 100, cents % 100)
}
