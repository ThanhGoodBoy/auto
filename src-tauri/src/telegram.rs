/// telegram.rs — Telegram Bot API helpers.
/// Uses reqwest directly (no telegram-specific crates needed).
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::config::Config;
use crate::zip_utils::zip_bytes;

// ─── Telegram response shapes ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct TgResponse<T> {
    ok:     bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TgFile {
    file_path: Option<String>,
}

#[derive(Deserialize)]
struct TgDocument {
    file_id: String,
}

#[derive(Deserialize)]
struct TgMessage {
    message_id: i64,
    document:   Option<TgDocument>,
}

// ─── Public API ────────────────────────────────────────────────────────────────

/// Send one part to Telegram. Returns (message_id, file_id).
pub async fn send_part(
    client:   &Client,
    cfg:      &Config,
    tg_token: &str,
    chat_id:  &str,
    buf_data: &[u8],
    part_num: u32,
    filename: &str,
    caption:  &str,
) -> Result<(i64, String)> {
    let part_name = format!("{filename}.part{part_num}");
    let zip_name  = format!("{part_name}.zip");
    let zip_data  = tokio::task::spawn_blocking({
        let data = buf_data.to_vec();
        let pname = part_name.clone();
        let level = cfg.zip_compress_level;
        move || zip_bytes(&data, &pname, level)
    }).await??;

    let zip_size = zip_data.len() as u64;
    info!("  📨 Telegram part {part_num}: zip={:.1}MB", zip_size as f64 / 1024.0 / 1024.0);

    if zip_size > cfg.tg_file_limit_bytes {
        anyhow::bail!(
            "Part {part_num} ({:.1}MB) exceeds Telegram limit ({:.0}MB). Reduce client_chunk_mb.",
            zip_size as f64 / 1024.0 / 1024.0,
            cfg.tg_file_limit_bytes as f64 / 1024.0 / 1024.0,
        );
    }

    let mut last_err = None;
    for attempt in 0..cfg.discord_send_retries {
        let form = reqwest::multipart::Form::new()
            .text("chat_id",  chat_id.to_string())
            .text("caption",  caption.to_string())
            .part(
                "document",
                reqwest::multipart::Part::bytes(zip_data.clone())
                    .file_name(zip_name.clone())
                    .mime_str("application/zip")?,
            );

        match client
            .post(format!("https://api.telegram.org/bot{tg_token}/sendDocument"))
            .multipart(form)
            .send()
            .await
        {
            Ok(resp) => {
                let body: TgResponse<TgMessage> = resp.json().await.context("parse Telegram response")?;
                if !body.ok {
                    let desc = body.description.unwrap_or_default();
                    last_err = Some(anyhow!("Telegram API error: {desc}"));
                    if attempt < cfg.discord_send_retries - 1 {
                        warn!("  ⚠️ Telegram retry {}/{}: {desc}", attempt+1, cfg.discord_send_retries);
                        let delay = cfg.discord_retry_base_s.pow(attempt);
                        tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                    }
                    continue;
                }
                let msg = body.result.ok_or_else(|| anyhow!("No result in Telegram response"))?;
                let file_id = msg.document
                    .as_ref()
                    .map(|d| d.file_id.clone())
                    .unwrap_or_default();
                return Ok((msg.message_id, file_id));
            }
            Err(e) => {
                last_err = Some(anyhow!("{e}"));
                if attempt < cfg.discord_send_retries - 1 {
                    warn!("  ⚠️ Telegram retry {}/{}: {e}", attempt+1, cfg.discord_send_retries);
                    let delay = cfg.discord_retry_base_s.pow(attempt);
                    tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Telegram send failed")))
}

/// Download one part from Telegram by file_id.
pub async fn download_part(
    client:   &Client,
    cfg:      &Config,
    tg_token: &str,
    file_id:  &str,
) -> Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..cfg.download_retry {
        match try_download(client, cfg, tg_token, file_id).await {
            Ok(data) => return Ok(data),
            Err(e) => {
                last_err = Some(e);
                if attempt < cfg.download_retry - 1 {
                    warn!("  ⚠️ Telegram download retry {}/{}", attempt+1, cfg.download_retry);
                    let delay = cfg.download_retry_base_s.pow(attempt);
                    tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("Telegram download failed")))
}

async fn try_download(client: &Client, cfg: &Config, tg_token: &str, file_id: &str) -> Result<Vec<u8>> {
    let timeout = std::time::Duration::from_secs(cfg.http_timeout_s);

    // getFile
    let r: TgResponse<TgFile> = client
        .get(format!("https://api.telegram.org/bot{tg_token}/getFile"))
        .query(&[("file_id", file_id)])
        .timeout(timeout)
        .send().await?
        .json().await?;

    let file_path = r.result
        .and_then(|f| f.file_path)
        .ok_or_else(|| anyhow!("No file_path for file_id {file_id}"))?;

    // Download
    let url = format!("https://api.telegram.org/file/bot{tg_token}/{file_path}");
    let data = client.get(&url).timeout(timeout).send().await?.bytes().await?;
    if data.is_empty() {
        anyhow::bail!("Empty response from Telegram CDN");
    }
    Ok(data.to_vec())
}
