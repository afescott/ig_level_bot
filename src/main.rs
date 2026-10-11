//! Level-based FTSE spread-bet bot for IG (demo) with risk-based sizing and server-side stops.
//!
//! Modes:
//! - `--mode mock` (default): replays synthetic ticks offline with paper fills, as fast as possible.
//! - `--mode ig`: polls IG demo prices (needs `IG_API_KEY`, `IG_IDENTIFIER`, `IG_PASSWORD`).
//!   With `--execute 0` (default) fills are simulated on IG prices; with `--execute 1` orders go
//!   to the IG **demo** account, with stop and target attached server-side.
//!
//! Create a file named `STOP` (or `--kill-file`) to close any position and halt in `ig` mode.

mod bars;
mod ig;
mod journal;
mod levels;
mod mock;
mod strategy;

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context};

use crate::ig::IgClient;
use crate::journal::{Entry, Journal};
use crate::strategy::{paper_exit, Action, Bias, Engine, StrategyConfig, Tick};

struct Flags(HashMap<String, String>);

impl Flags {
    fn parse() -> anyhow::Result<Self> {
        let mut map = HashMap::new();
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let Some(name) = k.strip_prefix("--") else { bail!("unexpected argument `{k}`") };
            let v = it.next().with_context(|| format!("missing value for `{k}`"))?;
            map.insert(name.to_string(), v);
        }
        Ok(Self(map))
    }

    fn get<T: FromStr>(&self, key: &str, default: T) -> anyhow::Result<T> {
        match self.0.get(key) {
            Some(v) => v.parse().map_err(|_| anyhow::anyhow!("invalid value for --{key}: `{v}`")),
            None => Ok(default),
        }
    }

    fn str(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_string())
    }
}

fn strategy_config(f: &Flags) -> anyhow::Result<StrategyConfig> {
    let bias = match f.str("bias", "short").as_str() {
        "short" => Bias::Short,
        "long" => Bias::Long,
        "both" => Bias::Both,
        other => bail!("invalid --bias `{other}` (short|long|both)"),
    };
    Ok(StrategyConfig {
        bias,
        bar_secs: f.get("bar-secs", 300)?,
        lookback_bars: f.get("lookback-bars", 288)?,
        swing: f.get("swing", 3)?,
        level_tol_pct: f.get("level-tol-pct", 0.08)?,
        min_touches: f.get("min-touches", 2)?,
        zone_pct: f.get("zone-pct", 0.05)?,
        reject_pct: f.get("reject-pct", 0.03)?,
        stop_buffer_pct: f.get("stop-buffer-pct", 0.05)?,
        min_rr: f.get("min-rr", 1.5)?,
        risk_gbp: f.get("risk-gbp", 20.0)?,
        min_size: f.get("min-size", 0.5)?,
        max_size: f.get("max-size", 20.0)?,
        max_spread: f.get("max-spread", 2.0)?,
        max_daily_loss_gbp: f.get("max-daily-loss-gbp", 60.0)?,
        max_trades_per_day: f.get("max-trades-per-day", 3)?,
        breakeven_r: f.get("breakeven-r", 1.0)?,
        trail_r: f.get("trail-r", 1.0)?,
    })
}

struct Runner {
    engine: Engine,
    journal: Journal,
    ig: Option<IgClient>,
    execute: bool,
    deal_id: Option<String>,
    ticks: u64,
}

impl Runner {
    fn log_close(&mut self, ts_ms: i64, px: f64, why: &str) {
        let p = self.engine.position.clone();
        if let (Some(p), Some((pnl, r))) = (p, self.engine.on_closed(px, why)) {
            self.journal.log(Entry {
                ts_ms,
                event: "CLOSE",
                dir: p.dir.ig(),
                price: px,
                size: p.size,
                stop: p.stop,
                limit: p.limit,
                pnl_gbp: pnl,
                reason: &format!("{why} ({r:+.2}R)"),
            });
        }
        self.deal_id = None;
    }

    async fn close_now(&mut self, t: &Tick, why: &str) {
        let Some(p) = self.engine.position.clone() else { return };
        let px = match (self.execute, self.ig.as_mut(), self.deal_id.clone()) {
            (true, Some(ig), Some(id)) => match ig.close(&id, p.dir, p.size).await {
                Ok(level) => level,
                Err(e) => {
                    self.error(t.ts_ms, &format!("close failed, position still open at IG: {e:#}"));
                    return;
                }
            },
            _ => t.exit_px(p.dir),
        };
        self.log_close(t.ts_ms, px, why);
    }

    fn error(&mut self, ts_ms: i64, msg: &str) {
        self.journal.log(Entry {
            ts_ms,
            event: "ERROR",
            dir: "",
            price: f64::NAN,
            size: 0.0,
            stop: f64::NAN,
            limit: f64::NAN,
            pnl_gbp: 0.0,
            reason: msg,
        });
    }

    async fn on_tick(&mut self, t: Tick) {
        self.ticks += 1;
        if let Some(p) = self.engine.position.clone() {
            if !self.execute {
                if let Some((px, why)) = paper_exit(&p, &t) {
                    self.log_close(t.ts_ms, px, why);
                }
            } else if self.ticks % 5 == 0 {
                if let (Some(ig), Some(id)) = (self.ig.as_mut(), self.deal_id.clone()) {
                    match ig.is_open(&id).await {
                        // Exact fill isn't fetched; book it at whichever resting level is nearer.
                        Ok(false) => {
                            let px = if (t.mid() - p.stop).abs() < (t.mid() - p.limit).abs() { p.stop } else { p.limit };
                            self.log_close(t.ts_ms, px, "closed by IG stop/limit (level approximate)");
                        }
                        Ok(true) => {}
                        Err(e) => self.error(t.ts_ms, &format!("position check failed: {e:#}")),
                    }
                }
            }
        }

        for action in self.engine.on_tick(&t) {
            match action {
                Action::Open { dir, size, stop, limit, reason } => {
                    let fill = match (self.execute, self.ig.as_mut()) {
                        (true, Some(ig)) => match ig.open(dir, size, stop, limit).await {
                            Ok(o) => {
                                self.deal_id = Some(o.deal_id);
                                o.level
                            }
                            Err(e) => {
                                self.error(t.ts_ms, &format!("open rejected: {e:#}"));
                                continue;
                            }
                        },
                        _ => t.entry_px(dir),
                    };
                    self.engine.on_opened(dir, size, fill, stop, limit, t.ts_ms);
                    self.journal.log(Entry {
                        ts_ms: t.ts_ms,
                        event: "OPEN",
                        dir: dir.ig(),
                        price: fill,
                        size,
                        stop,
                        limit,
                        pnl_gbp: 0.0,
                        reason: &reason,
                    });
                }
                Action::AmendStop { stop, reason } => {
                    let Some(p) = self.engine.position.clone() else { continue };
                    if let (true, Some(ig), Some(id)) = (self.execute, self.ig.as_mut(), self.deal_id.clone()) {
                        if let Err(e) = ig.amend(&id, stop, p.limit).await {
                            self.error(t.ts_ms, &format!("stop amend failed: {e:#}"));
                            continue;
                        }
                    }
                    self.engine.set_stop(stop);
                    self.journal.log(Entry {
                        ts_ms: t.ts_ms,
                        event: "AMEND",
                        dir: p.dir.ig(),
                        price: t.mid(),
                        size: p.size,
                        stop,
                        limit: p.limit,
                        pnl_gbp: 0.0,
                        reason: &reason,
                    });
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let f = Flags::parse()?;
    let cfg = strategy_config(&f)?;
    let mode = f.str("mode", "mock");
    let journal = Journal::open(&PathBuf::from(f.str("journal", "runs/journal.csv")));
    let mut runner = Runner {
        engine: Engine::new(cfg),
        journal,
        ig: None,
        execute: false,
        deal_id: None,
        ticks: 0,
    };

    match mode.as_str() {
        "mock" => {
            let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 5)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|d| d.and_utc().timestamp_millis())
                .unwrap_or(0);
            let feed = mock::MockFeed::new(
                start,
                f.get("mock-days", 10)?,
                f.get("mock-start-px", 8300.0)?,
                f.get("mock-spread", 1.0)?,
                f.get("seed", 42)?,
            );
            let mut last = None;
            for t in feed {
                runner.on_tick(t).await;
                last = Some(t);
            }
            if let Some(t) = last {
                runner.close_now(&t, "end of run").await;
            }
        }
        "ig" => {
            let mut ig = IgClient::from_env(f.str("epic", "IX.D.FTSE.DAILY.IP"), f.str("currency", "GBP"))?;
            ig.login().await.context("IG demo login")?;
            runner.execute = f.get::<u8>("execute", 0)? == 1;
            runner.ig = Some(ig);
            eprintln!(
                "IG demo: epic={} execute={} (orders {})",
                f.str("epic", "IX.D.FTSE.DAILY.IP"),
                runner.execute,
                if runner.execute { "sent to IG DEMO with server-side stops" } else { "simulated on IG prices" }
            );
            let kill = PathBuf::from(f.str("kill-file", "STOP"));
            let poll = Duration::from_secs(f.get("poll-secs", 3)?);
            let run_for: u64 = f.get("run-seconds", 0)?;
            let deadline = (run_for > 0).then(|| tokio::time::Instant::now() + Duration::from_secs(run_for));
            let mut interval = tokio::time::interval(poll);
            let mut last: Option<Tick> = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = tokio::signal::ctrl_c() => { eprintln!("ctrl-c"); break; }
                }
                if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
                    eprintln!("--run-seconds reached");
                    break;
                }
                let now = chrono::Utc::now().timestamp_millis();
                let tick = match runner.ig.as_mut().expect("ig").snapshot(now).await {
                    Ok(t) => t,
                    Err(e) => {
                        runner.error(now, &format!("price poll failed: {e:#}"));
                        continue;
                    }
                };
                if kill.exists() {
                    runner.close_now(&tick, "kill switch").await;
                    eprintln!("kill file {} found: halted", kill.display());
                    break;
                }
                runner.on_tick(tick).await;
                last = Some(tick);
            }
            if let (Some(t), false) = (last, runner.execute) {
                runner.close_now(&t, "end of run").await;
            } else if runner.engine.position.is_some() {
                eprintln!("position left open at IG with its server-side stop and target");
            }
        }
        other => bail!("invalid --mode `{other}` (mock|ig)"),
    }

    eprint!("SUMMARY {}", runner.engine.summary());
    Ok(())
}
