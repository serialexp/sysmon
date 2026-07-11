//! A fixed-capacity ring buffer of recent values, for sparklines.

use std::collections::VecDeque;

pub struct History {
    buf: VecDeque<f64>,
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, v: f64) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    pub fn iter(&self) -> impl Iterator<Item = &f64> {
        self.buf.iter()
    }
}
