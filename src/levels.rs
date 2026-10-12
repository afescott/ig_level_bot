//! Mechanical support/resistance: swing highs and lows clustered into price levels.
//!
//! A swing high is a bar whose high is the highest of the `swing` bars on each side (lows
//! mirrored). Swing points within `tol_pct` of each other merge into one level; a level needs
//! `min_touches` swing points to count. Whether a level acts as support or resistance depends
//! only on which side of it price is.

use crate::bars::Bar;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub price: f64,
    pub touches: usize,
}

fn swing_points(bars: &[Bar], swing: usize) -> Vec<f64> {
    let mut out = Vec::new();
    if bars.len() < 2 * swing + 1 {
        return out;
    }
    for i in swing..bars.len() - swing {
        let window = &bars[i - swing..=i + swing];
        let others = window.iter().enumerate().filter(|(j, _)| *j != swing).map(|(_, b)| b);
        let (mut is_high, mut is_low) = (true, true);
        for b in others {
            is_high &= bars[i].high > b.high;
            is_low &= bars[i].low < b.low;
        }
        if is_high {
            out.push(bars[i].high);
        }
        if is_low {
            out.push(bars[i].low);
        }
    }
    out
}

pub fn find_levels(bars: &[Bar], swing: usize, tol_pct: f64, min_touches: usize) -> Vec<Level> {
    let mut pts = swing_points(bars, swing);
    pts.sort_by(|a, b| a.total_cmp(b));
    let mut levels: Vec<Level> = Vec::new();
    let mut cluster: Vec<f64> = Vec::new();
    let flush = |cluster: &mut Vec<f64>, levels: &mut Vec<Level>| {
        if cluster.len() >= min_touches {
            levels.push(Level {
                price: cluster.iter().sum::<f64>() / cluster.len() as f64,
                touches: cluster.len(),
            });
        }
        cluster.clear();
    };
    for p in pts {
        if let Some(&first) = cluster.first() {
            if (p - first) / first > tol_pct / 100.0 {
                flush(&mut cluster, &mut levels);
            }
        }
        cluster.push(p);
    }
    flush(&mut cluster, &mut levels);
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(high: f64, low: f64) -> Bar {
        Bar {
            start_ms: 0,
            open: (high + low) / 2.0,
            high,
            low,
            close: (high + low) / 2.0,
        }
    }

    #[test]
    fn repeated_highs_form_one_level() {
        // Two peaks near 110 and a trough at 90.
        let highs = [100.0, 105.0, 110.0, 104.0, 100.0, 96.0, 100.0, 106.0, 110.2, 103.0, 100.0];
        let bars: Vec<Bar> = highs.iter().map(|&h| bar(h, if h == 96.0 { 90.0 } else { h - 5.0 })).collect();
        let levels = find_levels(&bars, 2, 0.5, 2);
        assert_eq!(levels.len(), 1, "{levels:?}");
        assert!((levels[0].price - 110.1).abs() < 1e-9);
        assert_eq!(levels[0].touches, 2);
    }

    #[test]
    fn single_touch_is_not_a_level() {
        let highs = [100.0, 105.0, 110.0, 104.0, 100.0];
        let bars: Vec<Bar> = highs.iter().map(|&h| bar(h, h - 5.0)).collect();
        assert!(find_levels(&bars, 2, 0.5, 2).is_empty());
    }
}
