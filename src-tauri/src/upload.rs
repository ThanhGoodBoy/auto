/// upload.rs — Upload session management and streaming sender.
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use serenity::{http::Http, model::id::{ChannelId, GuildId}};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, Mutex, Semaphore},
    task::JoinHandle,
    time::sleep,
};
use tracing::{info, warn};

use crate::{
    config::Config,
    discord_bot,
    storage::{current_datetime_iso, current_timestamp_ms, JsonStore, PartInfo, UploadSession},
    telegram,
    zip_utils::zip_bytes,
};

#[derive(Debug, Clone)]
pub struct SenderResult {
    pub method:      String,
    pub parts:       u32,
    pub parts_info:  Vec<PartInfo>,
    pub message_ids: Vec<i64>,
    pub jump_urls:   Vec<String>,
}

pub type ChunkTx = mpsc::Sender<(usize, Bytes)>;

pub struct SenderEntry {
    pub chunk_tx:  ChunkTx,
    pub result_rx: oneshot::Receiver<Result<SenderResult>>,
    pub handle:    JoinHandle<()>,
}

pub type SenderMap = Arc<Mutex<HashMap<String, SenderEntry>>>;

pub fn new_sender_map() -> SenderMap {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Session helpers ────────────────────────────────────────────────────────────

fn load_sessions(store: &JsonStore, file: &str) -> HashMap<String, UploadSession> {
    store.load_sessions(file)
}

fn save_sessions(store: &JsonStore, file: &str, sessions: &HashMap<String, UploadSession>) {
    if let Err(e) = store.save_sessions(file, sessions) {
        eprintln!("Failed to save sessions: {e}");
    }
}

pub fn create_session(
    store: &JsonStore, file: &str,
    filename: &str, file_size: u64, total_chunks: usize,
    folder_id: &str, message: &str,
) -> String {
    let hash_input = format!("{filename}{}", current_timestamp_ms());
    let digest = format!("{:x}", md5::compute(hash_input.as_bytes()));
    let session_id = digest[..12].to_string();
    let mut sessions = load_sessions(store, file);
    sessions.insert(session_id.clone(), UploadSession {
        session_id:      session_id.clone(),
        filename:        filename.to_string(),
        file_size,
        total_chunks,
        received_chunks: vec![],
        folder_id:       folder_id.to_string(),
        message:         message.to_string(),
        status:          "uploading".to_string(),
        created_at:      current_datetime_iso(),
        channel_id:      None,
        channel_name:    None,
        folder_name:     None,
        discord_result:  None,
    });
    save_sessions(store, file, &sessions);
    info!("📋 Session created: {session_id} ({filename}, {total_chunks} chunks)");
    session_id
}

pub fn get_session(store: &JsonStore, file: &str, id: &str) -> Option<UploadSession> {
    load_sessions(store, file).remove(id)
}

pub fn update_session(store: &JsonStore, file: &str, id: &str, f: impl FnOnce(&mut UploadSession)) {
    let mut sessions = load_sessions(store, file);
    if let Some(s) = sessions.get_mut(id) { f(s); }
    save_sessions(store, file, &sessions);
}

pub fn mark_chunk_received(store: &JsonStore, file: &str, id: &str, idx: usize) {
    update_session(store, file, id, |s| {
        if !s.received_chunks.contains(&idx) {
            s.received_chunks.push(idx);
            s.received_chunks.sort_unstable();
        }
    });
}

pub fn delete_session_record(store: &JsonStore, file: &str, id: &str) {
    let mut sessions = load_sessions(store, file);
    sessions.remove(id);
    save_sessions(store, file, &sessions);
}

// ── Sender task ────────────────────────────────────────────────────────────────

pub struct SenderArgs {
    pub session_id:   String,
    pub filename:     String,
    pub message:      String,
    pub total_chunks: usize,
    pub channel_id:   ChannelId,
    pub http:         Arc<Http>,
    pub guild_id:     GuildId,
    pub cfg:          Arc<Config>,
    pub tg_enabled:   bool,
    pub tg_token:     String,
    pub tg_chat_id:   String,
    pub chunk_rx:     mpsc::Receiver<(usize, Bytes)>,
    pub result_tx:    oneshot::Sender<Result<SenderResult>>,
}

pub fn spawn_sender(args: SenderArgs) -> JoinHandle<()> {
    tokio::spawn(async move {
        let res = streaming_sender(
            &args.session_id, &args.filename, &args.message,
            args.total_chunks, args.channel_id,
            &args.http, args.guild_id, &args.cfg,
            args.tg_enabled, &args.tg_token, &args.tg_chat_id,
            args.chunk_rx,
        ).await;
        let _ = args.result_tx.send(res);
    })
}

fn guild_filesize_limit(premium_tier: serenity::model::guild::PremiumTier) -> u64 {
    match premium_tier {
        serenity::model::guild::PremiumTier::Tier2 => 50  * 1024 * 1024,
        serenity::model::guild::PremiumTier::Tier3 => 100 * 1024 * 1024,
        _                                          => 10  * 1024 * 1024,
    }
}

async fn streaming_sender(
    _session_id:  &str,
    filename:     &str,
    message:      &str,
    total_chunks: usize,
    channel_id:   ChannelId,
    http:         &Arc<Http>,
    guild_id:     GuildId,
    cfg:          &Arc<Config>,
    tg_enabled:   bool,
    tg_token:     &str,
    tg_chat_id:   &str,
    mut chunk_rx: mpsc::Receiver<(usize, Bytes)>,
) -> Result<SenderResult> {
    let guild = guild_id.to_partial_guild(http).await.context("fetch guild")?;
    let guild_file_limit = guild_filesize_limit(guild.premium_tier);
    let discord_max = (guild_file_limit as f64 * cfg.discord_safe_ratio) as u64;
    let tg_max = if tg_enabled {
        (cfg.tg_file_limit_bytes as f64 * cfg.discord_safe_ratio) as u64
    } else { discord_max };
    let input_limit = discord_max.min(tg_max) as usize;

    info!("ℹ️  input_limit: {:.1}MB/part", input_limit as f64 / 1024.0 / 1024.0);

    let discord_sem = Arc::new(Semaphore::new(cfg.discord_parallel_sends));
    let tg_sem      = Arc::new(Semaphore::new(cfg.tg_parallel_sends));
    let reqwest_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.http_timeout_s))
        .build()?;

    let mut buffer: Vec<u8> = Vec::new();
    let mut pending_chunks: HashMap<usize, Bytes> = HashMap::new();
    let mut next_expected = 0usize;
    let mut total_parts = 0u32;
    let mut pending_tasks: Vec<(u32, JoinHandle<Result<PartInfo>>)> = vec![];
    let mut all_parts: Vec<PartInfo> = vec![];
    let mut message_ids = vec![];
    let mut jump_urls = vec![];

    info!("🚀 Streaming sender: {filename} ({total_chunks} chunks, dual={tg_enabled})");

    loop {
        // Drain channel without blocking
        loop {
            match chunk_rx.try_recv() {
                Ok((idx, data)) => { pending_chunks.insert(idx, data); }
                Err(_) => break,
            }
        }
        // Move ordered chunks into buffer
        while let Some(data) = pending_chunks.remove(&next_expected) {
            buffer.extend_from_slice(&data);
            next_expected += 1;
        }

        // Dispatch full parts
        while buffer.len() >= input_limit {
            total_parts += 1;
            let part_data: Vec<u8> = buffer.drain(..input_limit).collect();
            let use_tg = tg_enabled && (total_parts % 2 == 0);
            pending_tasks.push((total_parts, dispatch_part(
                total_parts, part_data, filename, message,
                channel_id, Arc::clone(http),
                Arc::clone(&discord_sem), Arc::clone(&tg_sem),
                Arc::clone(cfg), use_tg,
                tg_token.to_string(), tg_chat_id.to_string(),
                reqwest_client.clone(), guild_file_limit,
            )));
        }

        let all_in = next_expected >= total_chunks && pending_chunks.is_empty();

        // Flush final part
        if all_in && !buffer.is_empty() && pending_tasks.is_empty() {
            total_parts += 1;
            let part_data: Vec<u8> = buffer.drain(..).collect();
            let use_tg = tg_enabled && (total_parts % 2 == 0);
            pending_tasks.push((total_parts, dispatch_part(
                total_parts, part_data, filename, message,
                channel_id, Arc::clone(http),
                Arc::clone(&discord_sem), Arc::clone(&tg_sem),
                Arc::clone(cfg), use_tg,
                tg_token.to_string(), tg_chat_id.to_string(),
                reqwest_client.clone(), guild_file_limit,
            )));
        }

        // Collect finished tasks
        let mut still = vec![];
        for (pn, handle) in pending_tasks {
            if handle.is_finished() {
                let pi = handle.await.map_err(|e| anyhow!("{e}"))??;
                info!("  ✅ Part {} ({}) done", pi.part, pi.platform);
                message_ids.push(pi.message_id);
                if let Some(ref u) = pi.jump_url { jump_urls.push(u.clone()); }
                all_parts.push(pi);
            } else {
                still.push((pn, handle));
            }
        }
        pending_tasks = still;

        if all_in && buffer.is_empty() && pending_tasks.is_empty() { break; }

        if pending_tasks.is_empty() {
            // Block until next chunk arrives or channel closes
            match chunk_rx.recv().await {
                Some((idx, data)) => { pending_chunks.insert(idx, data); }
                None => {
                    // Flush remaining
                    if !buffer.is_empty() {
                        total_parts += 1;
                        let part_data: Vec<u8> = buffer.drain(..).collect();
                        let use_tg = tg_enabled && (total_parts % 2 == 0);
                        let h = dispatch_part(
                            total_parts, part_data, filename, message,
                            channel_id, Arc::clone(http),
                            Arc::clone(&discord_sem), Arc::clone(&tg_sem),
                            Arc::clone(cfg), use_tg,
                            tg_token.to_string(), tg_chat_id.to_string(),
                            reqwest_client.clone(), guild_file_limit,
                        );
                        let pi = h.await.map_err(|e| anyhow!("{e}"))??;
                        message_ids.push(pi.message_id);
                        if let Some(ref u) = pi.jump_url { jump_urls.push(u.clone()); }
                        all_parts.push(pi);
                    }
                    break;
                }
            }
        } else {
            sleep(Duration::from_millis(50)).await;
        }
    }

    all_parts.sort_by_key(|p| p.part);
    let method = if total_parts == 1 { "direct" }
        else if tg_enabled { "dual" }
        else { "split" };

    info!("✅ Streaming sender done: {filename} ({total_parts} parts, method={method})");
    Ok(SenderResult {
        method: method.to_string(),
        parts: total_parts,
        parts_info: all_parts,
        message_ids,
        jump_urls,
    })
}

#[allow(clippy::too_many_arguments)]
fn dispatch_part(
    part_num:    u32,
    part_data:   Vec<u8>,
    filename:    &str,
    message:     &str,
    channel_id:  ChannelId,
    http:        Arc<Http>,
    discord_sem: Arc<Semaphore>,
    tg_sem:      Arc<Semaphore>,
    cfg:         Arc<Config>,
    use_tg:      bool,
    tg_token:    String,
    tg_chat_id:  String,
    http_client: reqwest::Client,
    guild_limit: u64,
) -> JoinHandle<Result<PartInfo>> {
    let filename  = filename.to_string();
    let message   = message.to_string();
    tokio::spawn(async move {
        let caption   = build_caption(&filename, &message, part_num);
        let part_name = format!("{filename}.part{part_num}");

        if use_tg {
            let _permit = tg_sem.acquire().await?;
            let (msg_id, file_id) = telegram::send_part(
                &http_client, &cfg, &tg_token, &tg_chat_id,
                &part_data, part_num, &filename, &caption,
            ).await?;
            Ok(PartInfo {
                part: part_num, platform: "telegram".to_string(),
                message_id: msg_id, channel_id: None,
                file_id: Some(file_id), jump_url: None,
            })
        } else {
            let _permit = discord_sem.acquire().await?;
            let zip_data = tokio::task::spawn_blocking({
                let data = part_data.clone();
                let pname = part_name.clone();
                let level = cfg.zip_compress_level;
                move || zip_bytes(&data, &pname, level)
            }).await??;

            if zip_data.len() as u64 > guild_limit {
                anyhow::bail!("Part {part_num} ({:.1}MB) > guild limit. Reduce client_chunk_mb.",
                    zip_data.len() as f64 / 1024.0 / 1024.0);
            }

            let mut last_err = None;
            for attempt in 0..cfg.discord_send_retries {
                match discord_bot::send_part(
                    &http, channel_id,
                    zip_data.clone(), format!("{part_name}.zip"), caption.clone(),
                ).await {
                    Ok((msg_id, jump_url)) => return Ok(PartInfo {
                        part: part_num, platform: "discord".to_string(),
                        message_id: msg_id,
                        channel_id: Some(channel_id.get().to_string()),
                        file_id: None, jump_url: Some(jump_url),
                    }),
                    Err(e) => {
                        last_err = Some(e);
                        if attempt < cfg.discord_send_retries - 1 {
                            warn!("  ⚠️ Discord retry {}/{}", attempt+1, cfg.discord_send_retries);
                            sleep(Duration::from_secs(cfg.discord_retry_base_s.pow(attempt))).await;
                        }
                    }
                }
            }
            Err(last_err.unwrap_or_else(|| anyhow!("Discord send failed")))
        }
    })
}

fn build_caption(filename: &str, message: &str, part_num: u32) -> String {
    let mut c = format!("✂️ `{filename}` — Phần {part_num}");
    if !message.is_empty() && part_num == 1 { c.push('\n'); c.push_str(message); }
    c
}
