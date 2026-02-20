/// download.rs — Download and merge file parts from Discord / Telegram.
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use serenity::http::Http;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::{
    config::Config,
    discord_bot,
    storage::{FileRecord, PartInfo},
    telegram,
    zip_utils::unzip_or_raw,
};

/// Build a normalized parts list from a FileRecord (handles legacy format).
pub fn normalize_parts(record: &FileRecord) -> Vec<PartInfo> {
    if !record.parts_info.is_empty() {
        return record.parts_info.clone();
    }
    // Legacy: all Discord, using flat message_ids
    record.message_ids.iter().enumerate().map(|(i, &mid)| PartInfo {
        part:       (i + 1) as u32,
        platform:   "discord".to_string(),
        message_id: mid,
        channel_id: Some(record.channel_id.clone()),
        file_id:    None,
        jump_url:   None,
    }).collect()
}

/// Download one part (Discord or Telegram) and unzip it.
pub async fn fetch_part(
    info:       &PartInfo,
    http:       &Arc<Http>,
    cfg:        &Config,
    tg_client:  &reqwest::Client,
    tg_token:   &str,
) -> Result<Vec<u8>> {
    let raw = if info.platform == "telegram" {
        let file_id = info.file_id.as_deref()
            .ok_or_else(|| anyhow!("Telegram part {} has no file_id", info.part))?;
        telegram::download_part(tg_client, cfg, tg_token, file_id).await?
    } else {
        // Discord
        let channel_id_str = info.channel_id.as_deref()
            .ok_or_else(|| anyhow!("Discord part {} has no channel_id", info.part))?;
        let channel_id: u64 = channel_id_str.parse()
            .context("parse channel_id")?;
        let msg_id: u64 = info.message_id as u64;

        let url = discord_bot::fetch_attachment_url(http, channel_id, msg_id).await?;
        download_url(cfg, &url).await?
    };
    unzip_or_raw(raw)
}

async fn download_url(cfg: &Config, url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_s))
        .build()?;

    let mut last_err = None;
    for attempt in 0..cfg.download_retry {
        match client.get(url).send().await {
            Ok(resp) => {
                let data = resp.bytes().await?;
                if data.is_empty() {
                    last_err = Some(anyhow!("Empty response"));
                } else {
                    return Ok(data.to_vec());
                }
            }
            Err(e) => {
                last_err = Some(anyhow!("{e}"));
            }
        }
        if attempt < cfg.download_retry - 1 {
            let delay = cfg.download_retry_base_s.pow(attempt);
            sleep(Duration::from_secs(delay)).await;
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("Download failed")))
}

/// Merge all parts into a single byte stream.
/// Returns an async generator-style channel receiver for streaming.
pub async fn merge_to_channel(
    record:    FileRecord,
    http:      Arc<Http>,
    cfg:       Arc<Config>,
    tg_token:  String,
) -> tokio::sync::mpsc::Receiver<Result<Bytes>> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let tg_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.http_timeout_s))
            .build()
            .unwrap();

        let parts = normalize_parts(&record);
        let total = parts.len();

        for (i, part_info) in parts.into_iter().enumerate() {
            match fetch_part(&part_info, &http, &cfg, &tg_client, &tg_token).await {
                Ok(data) => {
                    info!("  ✅ Part {}/{} ({}) — {:.1}MB", i+1, total, part_info.platform,
                        data.len() as f64 / 1024.0 / 1024.0);
                    // Stream in read_buffer_bytes chunks
                    let buf_size = cfg.read_buffer_bytes;
                    let mut offset = 0;
                    while offset < data.len() {
                        let end = (offset + buf_size).min(data.len());
                        if tx.send(Ok(Bytes::copy_from_slice(&data[offset..end]))).await.is_err() {
                            return;
                        }
                        offset = end;
                    }
                    if cfg.part_delay_ms > 0 {
                        sleep(Duration::from_millis(cfg.part_delay_ms)).await;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    rx
}
