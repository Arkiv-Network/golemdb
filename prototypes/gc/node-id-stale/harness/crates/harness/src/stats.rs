//! Per-call latency histograms and counters. One per thread, merged at the end.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

#[derive(Default)]
pub struct Stats {
    latency: BTreeMap<&'static str, Histogram<u64>>,
    counters: BTreeMap<&'static str, u64>,
}

impl Stats {
    pub fn time<T>(&mut self, op: &'static str, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let out = f();
        self.record(op, start.elapsed());
        out
    }

    pub fn record(&mut self, op: &'static str, elapsed: Duration) {
        let micros = (elapsed.as_micros() as u64).max(1);
        self.latency
            .entry(op)
            .or_insert_with(|| Histogram::new(3).expect("histogram"))
            .record(micros)
            .expect("auto-resizing histogram");
    }

    pub fn count(&mut self, counter: &'static str, n: u64) {
        *self.counters.entry(counter).or_default() += n;
    }

    pub fn get(&self, counter: &str) -> u64 {
        self.counters.get(counter).copied().unwrap_or(0)
    }

    pub fn p50_ms(&self, op: &str) -> f64 {
        self.latency
            .get(op)
            .map_or(f64::NAN, |h| h.value_at_quantile(0.5) as f64 / 1000.0)
    }

    pub fn merge(&mut self, other: Stats) {
        for (op, h) in other.latency {
            self.latency
                .entry(op)
                .or_insert_with(|| Histogram::new(3).expect("histogram"))
                .add(h)
                .expect("merge");
        }
        for (counter, n) in other.counters {
            self.count(counter, n);
        }
    }

    pub fn print_latency(&self) {
        println!(
            "  {:<12} {:>8} {:>10} {:>10} {:>10}",
            "call", "count", "p50 ms", "p99 ms", "max ms"
        );
        for (op, h) in &self.latency {
            let ms = |q: f64| h.value_at_quantile(q) as f64 / 1000.0;
            println!(
                "  {op:<12} {:>8} {:>10.3} {:>10.3} {:>10.3}",
                h.len(),
                ms(0.5),
                ms(0.99),
                h.max() as f64 / 1000.0
            );
        }
    }
}
