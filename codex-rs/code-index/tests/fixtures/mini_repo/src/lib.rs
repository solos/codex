pub struct Counter {
    value: usize,
}

impl Counter {
    pub fn increment(&mut self) -> usize {
        self.value += 1;
        self.value
    }
}
