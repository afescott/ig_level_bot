//! Fixed-interval OHLC bars built from tick mids.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub start_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

pub struct BarBuilder {
    interval_ms: i64,
    current: Option<Bar>,
}

impl BarBuilder {
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval_ms: interval_secs.max(1) as i64 * 1_000,
            current: None,
        }
    }

    /// Add a price; returns the previous bar once a tick lands in a new interval.
    pub fn push(&mut self, ts_ms: i64, px: f64) -> Option<Bar> {
        let start = ts_ms.div_euclid(self.interval_ms) * self.interval_ms;
        match self.current.as_mut() {
            Some(bar) if bar.start_ms == start => {
                bar.high = bar.high.max(px);
                bar.low = bar.low.min(px);
                bar.close = px;
                None
            }
            _ => self.current.replace(Bar {
                start_ms: start,
                open: px,
                high: px,
                low: px,
                close: px,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_bar_when_interval_rolls() {
        let mut b = BarBuilder::new(60);
        assert_eq!(b.push(0, 10.0), None);
        assert_eq!(b.push(10_000, 12.0), None);
        assert_eq!(b.push(20_000, 9.0), None);
        assert_eq!(b.push(59_000, 11.0), None);
        let bar = b.push(60_000, 11.5).expect("completed");
        assert_eq!(
            bar,
            Bar {
                start_ms: 0,
                open: 10.0,
                high: 12.0,
                low: 9.0,
                close: 11.0
            }
        );
    }
}
