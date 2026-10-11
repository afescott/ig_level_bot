//! Minimal IG REST client, **demo environment only**.
//!
//! Positions are opened with `stopLevel` and `limitLevel` attached, so IG holds the stop and
//! target server-side and they protect the position even if this process dies.

use anyhow::{anyhow, bail, Context};
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};

use crate::strategy::{Dir, Tick};

const DEMO_BASE: &str = "https://demo-api.ig.com/gateway/deal";

pub struct IgClient {
    http: reqwest::Client,
    api_key: String,
    identifier: String,
    password: String,
    cst: String,
    security_token: String,
    pub epic: String,
    currency: String,
}

pub struct Opened {
    pub deal_id: String,
    pub level: f64,
}

impl IgClient {
    pub fn from_env(epic: String, currency: String) -> anyhow::Result<Self> {
        let var = |k: &str| std::env::var(k).with_context(|| format!("missing env var {k}"));
        Ok(Self {
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?,
            api_key: var("IG_API_KEY")?,
            identifier: var("IG_IDENTIFIER")?,
            password: var("IG_PASSWORD")?,
            cst: String::new(),
            security_token: String::new(),
            epic,
            currency,
        })
    }

    pub async fn login(&mut self) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!("{DEMO_BASE}/session"))
            .header("X-IG-API-KEY", &self.api_key)
            .header("Version", "2")
            .header("Accept", "application/json; charset=UTF-8")
            .json(&json!({ "identifier": self.identifier, "password": self.password }))
            .send()
            .await?;
        let status = resp.status();
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let (cst, xst) = (header("CST"), header("X-SECURITY-TOKEN"));
        if !status.is_success() {
            bail!("IG login failed: {status} {}", resp.text().await.unwrap_or_default());
        }
        self.cst = cst.ok_or_else(|| anyhow!("IG login: no CST header"))?;
        self.security_token = xst.ok_or_else(|| anyhow!("IG login: no X-SECURITY-TOKEN header"))?;
        Ok(())
    }

    /// Authenticated request; logs in again once if the session expired.
    async fn call(&mut self, method: Method, path: &str, version: u8, body: Option<Value>, delete_via_post: bool) -> anyhow::Result<Value> {
        for attempt in 0..2 {
            let mut req = self
                .http
                .request(method.clone(), format!("{DEMO_BASE}{path}"))
                .header("X-IG-API-KEY", &self.api_key)
                .header("CST", &self.cst)
                .header("X-SECURITY-TOKEN", &self.security_token)
                .header("Version", version.to_string())
                .header("Accept", "application/json; charset=UTF-8");
            if delete_via_post {
                req = req.header("_method", "DELETE");
            }
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.login().await?;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                bail!("IG {method} {path}: {status} {text}");
            }
            return Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text)? });
        }
        unreachable!()
    }

    pub async fn snapshot(&mut self, ts_ms: i64) -> anyhow::Result<Tick> {
        let path = format!("/markets/{}", self.epic);
        let v = self.call(Method::GET, &path, 3, None, false).await?;
        let s = &v["snapshot"];
        let (bid, offer) = (s["bid"].as_f64(), s["offer"].as_f64());
        let (Some(bid), Some(offer)) = (bid, offer) else {
            bail!("no prices (marketStatus={})", s["marketStatus"]);
        };
        Ok(Tick {
            ts_ms,
            bid,
            offer,
            tradeable: s["marketStatus"].as_str() == Some("TRADEABLE"),
        })
    }

    async fn confirm(&mut self, deal_reference: &str) -> anyhow::Result<Value> {
        let v = self.call(Method::GET, &format!("/confirms/{deal_reference}"), 1, None, false).await?;
        if v["dealStatus"].as_str() != Some("ACCEPTED") {
            bail!("deal {deal_reference} {}: {}", v["dealStatus"], v["reason"]);
        }
        Ok(v)
    }

    pub async fn open(&mut self, dir: Dir, size: f64, stop: f64, limit: f64) -> anyhow::Result<Opened> {
        let body = json!({
            "epic": self.epic,
            "expiry": "DFB",
            "direction": dir.ig(),
            "size": (size * 100.0).round() / 100.0,
            "orderType": "MARKET",
            "currencyCode": self.currency,
            "forceOpen": true,
            "guaranteedStop": false,
            "stopLevel": (stop * 10.0).round() / 10.0,
            "limitLevel": (limit * 10.0).round() / 10.0,
        });
        let v = self.call(Method::POST, "/positions/otc", 2, Some(body), false).await?;
        let reference = v["dealReference"].as_str().ok_or_else(|| anyhow!("no dealReference"))?.to_string();
        let c = self.confirm(&reference).await?;
        Ok(Opened {
            deal_id: c["dealId"].as_str().unwrap_or_default().to_string(),
            level: c["level"].as_f64().unwrap_or(f64::NAN),
        })
    }

    pub async fn amend(&mut self, deal_id: &str, stop: f64, limit: f64) -> anyhow::Result<()> {
        let body = json!({
            "stopLevel": (stop * 10.0).round() / 10.0,
            "limitLevel": (limit * 10.0).round() / 10.0,
            "trailingStop": false,
        });
        let v = self.call(Method::PUT, &format!("/positions/otc/{deal_id}"), 2, Some(body), false).await?;
        if let Some(r) = v["dealReference"].as_str() {
            self.confirm(&r.to_string()).await?;
        }
        Ok(())
    }

    /// Market close; returns the fill level.
    pub async fn close(&mut self, deal_id: &str, dir: Dir, size: f64) -> anyhow::Result<f64> {
        let opposite = match dir {
            Dir::Long => Dir::Short,
            Dir::Short => Dir::Long,
        };
        let body = json!({
            "dealId": deal_id,
            "direction": opposite.ig(),
            "size": (size * 100.0).round() / 100.0,
            "orderType": "MARKET",
        });
        let v = self.call(Method::POST, "/positions/otc", 1, Some(body), true).await?;
        let reference = v["dealReference"].as_str().ok_or_else(|| anyhow!("no dealReference"))?.to_string();
        let c = self.confirm(&reference).await?;
        Ok(c["level"].as_f64().unwrap_or(f64::NAN))
    }

    /// False once IG has closed the position (stop or limit hit server-side).
    pub async fn is_open(&mut self, deal_id: &str) -> anyhow::Result<bool> {
        let v = self.call(Method::GET, "/positions", 2, None, false).await?;
        Ok(v["positions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["position"]["dealId"].as_str() == Some(deal_id)))
    }
}
