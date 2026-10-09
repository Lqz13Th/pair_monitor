use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::Value;

pub const HL_INFO: &str = "https://api.hyperliquid.xyz/info";
pub const LIGHTER_API: &str = "https://mainnet.zklighter.elliot.ai/api/v1";

pub async fn hl_info(http: &reqwest::Client, body: Value) -> anyhow::Result<Value> {
    let kind = body["type"].as_str().unwrap_or_default().to_string();
    let r = http
        .post(HL_INFO)
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("hl {kind}"))?;
    if !r.status().is_success() {
        bail!("hl {kind}: HTTP {}", r.status());
    }
    Ok(r.json().await?)
}

pub async fn lighter_get(http: &reqwest::Client, path_and_query: &str) -> anyhow::Result<Value> {
    let r = http
        .get(format!("{LIGHTER_API}/{path_and_query}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("lighter {path_and_query}"))?;
    let v: Value = r.json().await?;
    if v["code"].as_i64().is_some_and(|c| c != 200 && c != 0) {
        bail!("lighter {path_and_query}: {v}");
    }
    Ok(v)
}

/// A number sent as a JSON number or a decimal string.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn num0(v: &Value) -> f64 {
    num(v).unwrap_or_default()
}

/// Adverse move left before liquidation, % of mark; None without a liquidation price.
pub fn liq_dist_pct(size: f64, mark: f64, liq: Option<f64>) -> Option<f64> {
    let liq = liq.filter(|l| *l > 0.0)?;
    if mark <= 0.0 || size == 0.0 {
        return None;
    }
    Some(if size > 0.0 { mark - liq } else { liq - mark } / mark * 100.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn numbers_and_liquidation_distance() {
        assert_eq!(num(&json!("1.5")), Some(1.5));
        assert_eq!(num(&json!(2)), Some(2.0));
        assert_eq!(num(&json!(null)), None);
        assert_eq!(liq_dist_pct(1.0, 100.0, Some(80.0)), Some(20.0));
        assert_eq!(liq_dist_pct(-1.0, 100.0, Some(110.0)), Some(10.0));
        assert_eq!(liq_dist_pct(1.0, 100.0, Some(0.0)), None);
        assert_eq!(liq_dist_pct(0.0, 100.0, Some(80.0)), None);
    }
}
