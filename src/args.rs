use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Offline synthetic ticks with paper fills.
    Mock,
    /// Live prices from the IG demo API.
    Ig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BiasArg {
    Short,
    Long,
    Both,
}

/// Level-based FTSE spread-bet bot for the IG demo account.
#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    #[arg(long, value_enum, default_value_t = Mode::Mock)]
    pub mode: Mode,

    /// In ig mode: false = paper fills on IG prices, true = real orders on the IG DEMO account.
    #[arg(long, default_value_t = false)]
    pub execute: bool,

    #[arg(long, value_enum, default_value_t = BiasArg::Short)]
    pub bias: BiasArg,

    // IG credentials
    #[arg(long, env = "IG_API_KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    #[arg(long, env = "IG_IDENTIFIER", hide_env_values = true)]
    pub identifier: Option<String>,

    #[arg(long, env = "IG_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    // IG market and polling
    #[arg(long, default_value = "IX.D.FTSE.DAILY.IP")]
    pub epic: String,

    #[arg(long, default_value = "GBP")]
    pub currency: String,

    #[arg(long, default_value_t = 3)]
    pub poll_secs: u64,

    /// 0 = run until Ctrl-C.
    #[arg(long, default_value_t = 0)]
    pub run_seconds: u64,

    #[arg(long, default_value = "STOP")]
    pub kill_file: PathBuf,

    // Risk
    #[arg(long, default_value_t = 20.0)]
    pub risk_gbp: f64,

    #[arg(long, default_value_t = 60.0)]
    pub max_daily_loss_gbp: f64,

    #[arg(long, default_value_t = 3)]
    pub max_trades_per_day: u32,

    #[arg(long, default_value_t = 1.5)]
    pub min_rr: f64,

    #[arg(long, default_value_t = 0.5)]
    pub min_size: f64,

    #[arg(long, default_value_t = 20.0)]
    pub max_size: f64,

    #[arg(long, default_value_t = 2.0)]
    pub max_spread: f64,

    // Levels
    #[arg(long, default_value_t = 300)]
    pub bar_secs: i64,

    #[arg(long, default_value_t = 288)]
    pub lookback_bars: usize,

    #[arg(long, default_value_t = 3)]
    pub swing: usize,

    #[arg(long, default_value_t = 0.08)]
    pub level_tol_pct: f64,

    #[arg(long, default_value_t = 2)]
    pub min_touches: usize,

    // Entry and exit
    #[arg(long, default_value_t = 0.05)]
    pub zone_pct: f64,

    #[arg(long, default_value_t = 0.03)]
    pub reject_pct: f64,

    #[arg(long, default_value_t = 0.05)]
    pub stop_buffer_pct: f64,

    #[arg(long, default_value_t = 1.0)]
    pub breakeven_r: f64,

    #[arg(long, default_value_t = 1.0)]
    pub trail_r: f64,

    // Mock feed
    #[arg(long, default_value_t = 10)]
    pub mock_days: u32,

    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    #[arg(long, default_value_t = 8300.0)]
    pub mock_start_px: f64,

    #[arg(long, default_value_t = 1.0)]
    pub mock_spread: f64,

    #[arg(long, default_value = "runs/journal.csv")]
    pub journal: PathBuf,
}
