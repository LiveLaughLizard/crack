use crate::compute_total;

pub struct Cart {
    pub lines: Vec<(u64, u64)>,
}

impl Cart {
    pub fn total(&self) -> u64 {
        compute_total(&self.lines)
    }
}
