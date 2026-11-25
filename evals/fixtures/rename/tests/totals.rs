use rename::cart::Cart;
use rename::compute_total;

#[test]
fn totals_agree() {
    let lines = vec![(250, 2), (750, 1)];
    assert_eq!(compute_total(&lines), 1250);
    assert_eq!(Cart { lines: lines.clone() }.total(), 1250);
    assert_eq!(rename::report::summary(&lines), "3 items, $12.50");
}
