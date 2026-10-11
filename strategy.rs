//! Level-rejection strategy with risk-based sizing.
//!
//! **Entry (short bias):** a completed bar trades up into a resistance level (high within
//! `zone_pct` of it), then closes bearish at least `reject_pct` below it. Stop goes
//! `stop_buffer_pct` above the level; the target is the nearest support below, and the trade is
//! skipped unless that support is at least `min_rr` × risk away (a closer support would be in the
//! way). Long bias mirrors this off support.
//!
//! **Sizing:** spread-bet size (£ per point) = `risk_gbp` / stop distance, so a stopped-out trade
//! loses about `risk_gbp` (more if price gaps through the stop).
//!
//! **Management:** at `breakeven_r` × risk in profit the stop moves to entry; after that it trails
//! `trail_r` × risk behind the best price, amended only in steps of at least 0.25R.
//!
//! **Limits:** max trades per day, daily loss limit, max spread, market must be tradeable.
//!
//! The engine never talks to a broker: it returns [`Action`]s, and the caller reports fills back
//! with [`Engine::on_opened`], [`Engine::set_stop`] and [`Engine::on_closed`].

use std::collections::{BTreeMap, VecDeque};

use crate::bars::{Bar, BarBuilder};
use crate::levels::{find_levels, Level};

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Long,
    Short,
}

impl Dir {
    pub fn sign(self) -> f64 {
        match self {
            Dir::Long => 1.0,
            Dir::Short => -1.0,
        }
    }
    pub fn ig(self) -> &'static str {
        match self {
            Dir::Long => "BUY",
            Dir::Short => "SELL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    Short,
    Long,
    Both,
}

impl Bias {
    fn allows(self, d: Dir) -> bool {
        matches!((self, d), (Bias::Both, _) | (Bias::Short, Dir::Short) | (Bias::Long, Dir::Long))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub ts_ms: i64,
    pub bid: f64,
    pub offer: f64,
    pub tradeable: bool,
}

impl Tick {
    pub fn mid(&self) -> f64 {
        (self.bid + self.offer) / 2.0
    }
    pub fn spread(&self) -> f64 {
        self.offer - self.bid
    }
    /// Price a market order fills at when opening in `dir` (closing is the opposite side).
    pub fn entry_px(&self, dir: Dir) -> f64 {
        match dir {
            Dir::Long => self.offer,
            Dir::Short => self.bid,
        }
    }
    pub fn exit_px(&self, dir: Dir) -> f64 {
        match dir {
            Dir::Long => self.bid,
            Dir::Short => self.offer,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StrategyConfig {
    pub bias: Bias,
    pub bar_secs: u64,
    pub lookback_bars: usize,
    pub swing: usize,
    pub level_tol_pct: f64,
    pub min_touches: usize,
    pub zone_pct: f64,
    pub reject_pct: f64,
    pub stop_buffer_pct: f64,
    pub min_rr: f64,
    pub risk_gbp: f64,
    pub min_size: f64,
    pub max_size: f64,
    pub max_spread: f64,
    pub max_daily_loss_gbp: f64,
    pub max_trades_per_day: u32,
    pub breakeven_r: f64,
    pub trail_r: f64,
}

#[derive(Debug, Clone)]
pub struct Position {
    pub dir: Dir,
    pub size: f64,
    pub entry: f64,
    pub stop: f64,
    pub limit: f64,
    pub risk_pts: f64,
    /// Best mid seen in the trade's favour.
    pub best: f64,
    pub at_breakeven: bool,
    pub opened_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Open { dir: Dir, size: f64, stop: f64, limit: f64, reason: String },
    AmendStop { stop: f64, reason: String },
}

#[derive(Debug, Default)]
pub struct Stats {
    pub trades: u32,
    pub wins: u32,
    pub pnl_gbp: f64,
    pub sum_r: f64,
    pub max_dd_gbp: f64,
    peak: f64,
    pub by_exit: BTreeMap<String, u32>,
    pub skipped: BTreeMap<String, u32>,
}

pub struct Engine {
    cfg: StrategyConfig,
    bars: BarBuilder,
    history: VecDeque<Bar>,
    pub position: Option<Position>,
    day: i64,
    realised_today: f64,
    trades_today: u32,
    pub stats: Stats,
}

fn round_down(x: f64, step: f64) -> f64 {
    (x / step).floor() * step
}

impl Engine {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self {
            bars: BarBuilder::new(cfg.bar_secs),
            cfg,
            history: VecDeque::new(),
            position: None,
            day: i64::MIN,
            realised_today: 0.0,
            trades_today: 0,
            stats: Stats::default(),
        }
    }

    pub fn on_tick(&mut self, t: &Tick) -> Vec<Action> {
        let mut out = Vec::new();
        let day = t.ts_ms.div_euclid(DAY_MS);
        if day != self.day {
            self.day = day;
            self.realised_today = 0.0;
            self.trades_today = 0;
        }
        if let Some(a) = self.manage(t) {
            out.push(a);
        }
        if let Some(bar) = self.bars.push(t.ts_ms, t.mid()) {
            if self.position.is_none() {
                if let Some(a) = self.entry(&bar, t) {
                    out.push(a);
                }
            }
            self.history.push_back(bar);
            while self.history.len() > self.cfg.lookback_bars {
                self.history.pop_front();
            }
        }
        out
    }

    fn skip(&mut self, why: &str) -> Option<Action> {
        *self.stats.skipped.entry(why.to_string()).or_default() += 1;
        None
    }

    /// Signal on the bar that just completed, using levels from the bars before it.
    fn entry(&mut self, bar: &Bar, t: &Tick) -> Option<Action> {
        let c = self.cfg;
        let hist: Vec<Bar> = self.history.iter().copied().collect();
        let levels = find_levels(&hist, c.swing, c.level_tol_pct, c.min_touches);
        if levels.is_empty() {
            return None;
        }
        let px = bar.close;
        let zone = px * c.zone_pct / 100.0;
        let reject = px * c.reject_pct / 100.0;

        let short = levels
            .iter()
            .filter(|l| l.price > px && bar.high >= l.price - zone && bar.close <= l.price - reject && bar.close < bar.open)
            .min_by(|a, b| (bar.high - a.price).abs().total_cmp(&(bar.high - b.price).abs()))
            .map(|l| (Dir::Short, *l));
        let long = levels
            .iter()
            .filter(|l| l.price < px && bar.low <= l.price + zone && bar.close >= l.price + reject && bar.close > bar.open)
            .min_by(|a, b| (bar.low - a.price).abs().total_cmp(&(bar.low - b.price).abs()))
            .map(|l| (Dir::Long, *l));
        let (dir, level) = match (short, long) {
            (Some(s), _) if c.bias.allows(Dir::Short) => s,
            (_, Some(l)) if c.bias.allows(Dir::Long) => l,
            (None, None) => return None,
            _ => return self.skip("against bias"),
        };

        if !t.tradeable {
            return self.skip("market not tradeable");
        }
        if t.spread() > c.max_spread {
            return self.skip("spread too wide");
        }
        if self.trades_today >= c.max_trades_per_day {
            return self.skip("max trades today");
        }
        if self.realised_today <= -c.max_daily_loss_gbp {
            return self.skip("daily loss limit");
        }

        let entry = t.entry_px(dir);
        let buffer = entry * c.stop_buffer_pct / 100.0;
        let stop = level.price - dir.sign() * buffer;
        let risk = dir.sign() * (entry - stop);
        if risk <= 0.0 {
            return self.skip("price already beyond stop");
        }
        let target = self.target(&levels, dir, entry, &level);
        let Some(target) = target else {
            return self.skip("no level to target");
        };
        let reward = dir.sign() * (target.price - entry);
        if reward < c.min_rr * risk {
            return self.skip("next level too close (R:R)");
        }
        let size = round_down((c.risk_gbp / risk).min(c.max_size), 0.01);
        if size < c.min_size {
            return self.skip("stop too wide for risk budget");
        }
        Some(Action::Open {
            dir,
            size,
            stop,
            limit: target.price,
            reason: format!(
                "{} rejection at {:.1} ({} touches), entry~{:.1} stop {:.1} target {:.1} ({} touches) R:R {:.2}, £{:.2}/pt risks £{:.2}",
                if dir == Dir::Short { "resistance" } else { "support" },
                level.price,
                level.touches,
                entry,
                stop,
                target.price,
                target.touches,
                reward / risk,
                size,
                size * risk
            ),
        })
    }

    /// Nearest level beyond `entry` in the trade's direction (excluding the entry level).
    fn target(&self, levels: &[Level], dir: Dir, entry: f64, from: &Level) -> Option<Level> {
        levels
            .iter()
            .filter(|l| l.price != from.price && dir.sign() * (l.price - entry) > 0.0)
            .min_by(|a, b| (a.price - entry).abs().total_cmp(&(b.price - entry).abs()))
            .copied()
    }

    fn manage(&mut self, t: &Tick) -> Option<Action> {
        let c = self.cfg;
        let p = self.position.as_mut()?;
        let mid = t.mid();
        if p.dir.sign() * (mid - p.best) > 0.0 {
            p.best = mid;
        }
        let fav = p.dir.sign() * (p.best - p.entry);
        if !p.at_breakeven {
            if fav >= c.breakeven_r * p.risk_pts {
                p.at_breakeven = true;
                return Some(Action::AmendStop {
                    stop: p.entry,
                    reason: format!("+{:.1}R: stop to breakeven", fav / p.risk_pts),
                });
            }
            return None;
        }
        let trail = p.best - p.dir.sign() * c.trail_r * p.risk_pts;
        let improvement = p.dir.sign() * (trail - p.stop);
        (improvement >= 0.25 * p.risk_pts).then(|| Action::AmendStop {
            stop: trail,
            reason: format!("trail {:.1}R behind best {:.1}", c.trail_r, p.best),
        })
    }

    pub fn on_opened(&mut self, dir: Dir, size: f64, entry: f64, stop: f64, limit: f64, ts_ms: i64) {
        self.trades_today += 1;
        self.position = Some(Position {
            dir,
            size,
            entry,
            stop,
            limit,
            risk_pts: dir.sign() * (entry - stop),
            best: entry,
            at_breakeven: false,
            opened_ms: ts_ms,
        });
    }

    pub fn set_stop(&mut self, stop: f64) {
        if let Some(p) = self.position.as_mut() {
            p.stop = stop;
        }
    }

    /// Book the exit; returns `(pnl_gbp, r_multiple)`.
    pub fn on_closed(&mut self, exit_px: f64, reason: &str) -> Option<(f64, f64)> {
        let p = self.position.take()?;
        let pnl = p.size * p.dir.sign() * (exit_px - p.entry);
        let r = pnl / (p.size * p.risk_pts);
        let s = &mut self.stats;
        s.trades += 1;
        s.wins += (pnl > 0.0) as u32;
        s.pnl_gbp += pnl;
        s.sum_r += r;
        s.peak = s.peak.max(s.pnl_gbp);
        s.max_dd_gbp = s.max_dd_gbp.max(s.peak - s.pnl_gbp);
        *s.by_exit.entry(reason.to_string()).or_default() += 1;
        self.realised_today += pnl;
        Some((pnl, r))
    }

    pub fn summary(&self) -> String {
        let s = &self.stats;
        let n = s.trades.max(1) as f64;
        let mut out = format!(
            "trades={} wins={} ({:.0}%) pnl=£{:.2} avg={:+.2}R maxDD=£{:.2}\n  exits: {:?}\n",
            s.trades,
            s.wins,
            100.0 * s.wins as f64 / n,
            s.pnl_gbp,
            s.sum_r / n,
            s.max_dd_gbp,
            s.by_exit
        );
        if !s.skipped.is_empty() {
            out.push_str(&format!("  skipped signals: {:?}\n", s.skipped));
        }
        out
    }
}

/// Paper fills for a resting stop/limit, as IG would execute them server-side.
pub fn paper_exit(p: &Position, t: &Tick) -> Option<(f64, &'static str)> {
    let close = t.exit_px(p.dir);
    let stop_hit = p.dir.sign() * (close - p.stop) <= 0.0;
    let limit_hit = p.dir.sign() * (close - p.limit) >= 0.0;
    if stop_hit {
        // Fill at the market, which is at or beyond the stop if price gapped.
        let why = if p.at_breakeven { "trailing/breakeven stop" } else { "stop" };
        Some((close, why))
    } else if limit_hit {
        Some((p.limit, "target"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bias: Bias) -> StrategyConfig {
        StrategyConfig {
            bias,
            bar_secs: 60,
            lookback_bars: 500,
            swing: 2,
            level_tol_pct: 0.1,
            min_touches: 2,
            zone_pct: 0.05,
            reject_pct: 0.03,
            stop_buffer_pct: 0.05,
            min_rr: 1.5,
            risk_gbp: 20.0,
            min_size: 0.5,
            max_size: 50.0,
            max_spread: 2.0,
            max_daily_loss_gbp: 60.0,
            max_trades_per_day: 3,
            breakeven_r: 1.0,
            trail_r: 1.0,
        }
    }

    fn tick(ts_ms: i64, mid: f64) -> Tick {
        Tick {
            ts_ms,
            bid: mid - 0.5,
            offer: mid + 0.5,
            tradeable: true,
        }
    }

    /// Feed one bar per minute as four ticks: open, high, low, close.
    fn feed_bar(e: &mut Engine, minute: i64, o: f64, h: f64, l: f64, c: f64) -> Vec<Action> {
        let base = minute * 60_000;
        let mut out = Vec::new();
        for (k, px) in [o, h, l, c].into_iter().enumerate() {
            out.extend(e.on_tick(&tick(base + k as i64 * 10_000, px)));
        }
        out
    }

    /// Range 7900–8000: two peaks at 8000 and two troughs at 7900, then a rally into 8000.
    fn range_then_rally(e: &mut Engine) -> Vec<Action> {
        let path = [
            7950.0, 7975.0, 8000.0, 7975.0, 7950.0, 7925.0, 7900.0, 7925.0, 7950.0, 7975.0, 8000.0,
            7975.0, 7950.0, 7925.0, 7900.0, 7925.0, 7950.0, 7975.0,
        ];
        let mut m = 0;
        for &p in &path {
            feed_bar(e, m, p, p + 1.0, p - 1.0, p);
            m += 1;
        }
        // Rejection bar: opens 7990, pokes 8000, closes 7985 (bearish, 15 pts below level).
        let mut acts = feed_bar(e, m, 7990.0, 8000.5, 7984.0, 7985.0);
        // Next bar's first tick completes the rejection bar.
        acts.extend(e.on_tick(&tick((m + 1) * 60_000, 7985.0)));
        acts
    }

    #[test]
    fn short_on_resistance_rejection_with_risk_sized_position() {
        let mut e = Engine::new(cfg(Bias::Short));
        let acts = range_then_rally(&mut e);
        let open = acts.iter().find_map(|a| match a {
            Action::Open { dir, size, stop, limit, .. } => Some((*dir, *size, *stop, *limit)),
            _ => None,
        });
        let (dir, size, stop, limit) = open.expect("short signal");
        assert_eq!(dir, Dir::Short);
        // Level ≈ 8001 (swing highs at 8001); stop = level + 0.05% of entry.
        assert!(stop > 8001.0 && stop < 8006.0, "stop {stop}");
        // Target is the 7899 support.
        assert!((limit - 7899.0).abs() < 1.0, "limit {limit}");
        // £20 risk over ~20 pts → ~£1/pt.
        let risk_pts = stop - 7984.5;
        assert!((size * risk_pts - 20.0).abs() < 0.5, "size {size} risk {risk_pts}");
    }

    #[test]
    fn long_bias_ignores_resistance_rejection() {
        let mut e = Engine::new(cfg(Bias::Long));
        let acts = range_then_rally(&mut e);
        assert!(acts.iter().all(|a| !matches!(a, Action::Open { .. })));
        assert_eq!(e.stats.skipped.get("against bias"), Some(&1));
    }

    #[test]
    fn stop_moves_to_breakeven_then_trails() {
        let mut e = Engine::new(cfg(Bias::Short));
        e.on_opened(Dir::Short, 1.0, 8000.0, 8020.0, 7900.0, 0);
        // +1R (20 pts) in favour → breakeven.
        let a = e.on_tick(&tick(1_000, 7980.0));
        assert!(matches!(a.as_slice(), [Action::AmendStop { stop, .. }] if *stop == 8000.0));
        e.set_stop(8000.0);
        // Best 7970 → trail at 7990 (improves 10 pts = 0.5R ≥ 0.25R).
        let a = e.on_tick(&tick(2_000, 7970.0));
        assert!(matches!(a.as_slice(), [Action::AmendStop { stop, .. }] if (*stop - 7990.0).abs() < 1e-9));
    }

    #[test]
    fn daily_loss_limit_blocks_new_entries() {
        let mut e = Engine::new(StrategyConfig {
            max_trades_per_day: 10,
            ..cfg(Bias::Short)
        });
        e.on_tick(&tick(0, 8000.0));
        for _ in 0..3 {
            e.on_opened(Dir::Short, 1.0, 8000.0, 8020.0, 7900.0, 0);
            e.on_closed(8020.0, "stop");
        }
        assert!(e.realised_today <= -60.0);
        let acts = range_then_rally(&mut e);
        assert!(acts.iter().all(|a| !matches!(a, Action::Open { .. })));
        assert_eq!(e.stats.skipped.get("daily loss limit"), Some(&1));
    }

    #[test]
    fn paper_exit_fills_stop_at_market_when_gapped() {
        let p = Position {
            dir: Dir::Short,
            size: 1.0,
            entry: 8000.0,
            stop: 8020.0,
            limit: 7900.0,
            risk_pts: 20.0,
            best: 8000.0,
            at_breakeven: false,
            opened_ms: 0,
        };
        // Gap open at 8050: buy back at the offer, not the stop.
        let (px, why) = paper_exit(&p, &tick(0, 8050.0)).unwrap();
        assert_eq!((px, why), (8050.5, "stop"));
        let (px, why) = paper_exit(&p, &tick(0, 7890.0)).unwrap();
        assert_eq!((px, why), (7900.0, "target"));
    }
}
