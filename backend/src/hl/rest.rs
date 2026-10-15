//! Hyperliquid REST client (info endpoint).

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::sync::Mutex;

pub struct HlRest {
    client: reqwest::Client,
    url: String,
    lock: Mutex<()>,
}

impl HlRest {
    pub fn new(url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(12))
            .gzip(true)
            .build()
            .expect("reqwest client");
        Self {
            client,
            url: url.to_string(),
            lock: Mutex::new(()),
        }
    }

    async fn post(&self, body: &serde_json::Value) -> Result<reqwest::Response> {
        let _g = self.lock.lock().await; // serialize requests; be polite to the API
        let resp = self
            .client
            .post(&self.url)
            .json(body)
            .send()
            .await
            .context("HL info request failed")?;
        Ok(resp)
    }

    pub async fn post_json<T: for<'de> serde::Deserialize<'de>>(
        &self,
        body: &serde_json::Value,
    ) -> Result<T> {
        let resp = self.post(body).await?;
        let status = resp.status();
        let text = resp.text().await.context("read body")?;
        if !status.is_success() {
            anyhow::bail!("HL info HTTP {} : {}", status, &text[..text.len().min(200)]);
        }
        serde_json::from_str(&text).with_context(|| format!("decode HL response: {}", &text[..text.len().min(200)]))
    }

    pub async fn meta_and_asset_ctxs(
        &self,
    ) -> Result<(
        Vec<crate::hl::types::HlMetaAsset>,
        Vec<crate::hl::types::HlAssetCtx>,
    )> {
        let raw = self
            .post_json::<serde_json::Value>(&serde_json::json!({"type": "metaAndAssetCtxs"}))
            .await?;
        let arr = raw
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("metaAndAssetCtxs: expected array"))?;
        let meta: crate::hl::types::HlMeta =
            serde_json::from_value(arr.first().cloned().unwrap_or_default())?;
        let ctxs: Vec<crate::hl::types::HlAssetCtx> =
            serde_json::from_value(arr.get(1).cloned().unwrap_or_default())?;
        Ok((meta.universe, ctxs))
    }

    pub async fn candle_snapshot(
        &self,
        coin: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<crate::hl::types::HlCandle>> {
        self.post_json::<Vec<crate::hl::types::HlCandle>>(&serde_json::json!({
            "type": "candleSnapshot",
            "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms }
        }))
        .await
    }

    pub async fn user_fills(&self, user: &str) -> Result<Vec<crate::hl::types::HlUserFill>> {
        self.post_json::<Vec<crate::hl::types::HlUserFill>>(&serde_json::json!({
            "type": "userFills", "user": user
        }))
        .await
    }

    pub async fn clearinghouse_state(&self, user: &str) -> Result<Vec<crate::hl::types::HlClearinghouseState>> {
        self.post_json::<Vec<crate::hl::types::HlClearinghouseState>>(&serde_json::json!({
            "type": "clearinghouseState", "user": user
        }))
        .await
    }
}
