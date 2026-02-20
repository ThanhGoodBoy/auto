/// api.rs — All Axum route handlers.
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, io::Cursor};
use tokio::sync::oneshot;
use tracing::info;

use crate::{
    discord_bot,
    download,
    state::AppState,
    storage::{current_datetime_display, current_timestamp_ms, FileRecord, Folder},
    upload::{create_session, delete_session_record, get_session, mark_chunk_received,
             update_session, SenderArgs, SenderEntry},
};

// ── Error helper ───────────────────────────────────────────────────────────────

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": msg.into() }))).into_response()
}

// ── Health ─────────────────────────────────────────────────────────────────────

pub async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

// ── Folders ────────────────────────────────────────────────────────────────────

pub async fn get_folders(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "folders": st.store.load_folders(&st.cfg.folders_file) }))
}

pub async fn create_folder(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    if name.is_empty() { return err(StatusCode::BAD_REQUEST, "Tên folder không được trống"); }
    match discord_bot::get_or_create_category(&st.http, st.guild_id, &name).await {
        Ok(cat) => {
            let mut folders = st.store.load_folders(&st.cfg.folders_file);
            let folder = Folder {
                id:                  current_timestamp_ms(),
                name,
                discord_category_id: cat.id.get() as i64,
                created_at:          current_datetime_display(),
            };
            folders.insert(0, folder.clone());
            let _ = st.store.save_folders(&st.cfg.folders_file, &folders);
            Json(json!({ "success": true, "folder": folder })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_folder(State(st): State<AppState>, Path(folder_id): Path<i64>) -> impl IntoResponse {
    let mut folders = st.store.load_folders(&st.cfg.folders_file);
    if let Some(f) = folders.iter().find(|f| f.id == folder_id) {
        let _ = discord_bot::delete_category(&st.http, st.guild_id, f.discord_category_id as u64).await;
    }
    folders.retain(|f| f.id != folder_id);
    let _ = st.store.save_folders(&st.cfg.folders_file, &folders);
    Json(json!({ "success": true }))
}

// ── Files ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FolderQuery { folder_id: Option<String> }

#[derive(Deserialize)]
pub struct DeleteFileQuery { delete_channel: Option<bool> }

pub async fn get_files(State(st): State<AppState>, Query(q): Query<FolderQuery>) -> impl IntoResponse {
    let files = st.store.load_history(&st.cfg.history_file);
    let filtered: Vec<_> = if let Some(ref fid) = q.folder_id {
        if fid.is_empty() {
            files.into_iter().filter(|f| f.folder_id.is_none()).collect()
        } else {
            files.into_iter().filter(|f| {
                f.folder_id.as_ref().map(|v|
                    v.as_str().map(|s| s == fid).unwrap_or_else(|| v.to_string() == *fid)
                ).unwrap_or(false)
            }).collect()
        }
    } else {
        files.into_iter().filter(|f| f.folder_id.is_none()).collect()
    };
    Json(json!({ "files": filtered }))
}

pub async fn delete_file(
    State(st): State<AppState>,
    Path(file_id): Path<i64>,
    Query(q): Query<DeleteFileQuery>,
) -> impl IntoResponse {
    let mut history = st.store.load_history(&st.cfg.history_file);
    if q.delete_channel.unwrap_or(false) {
        if let Some(rec) = history.iter().find(|f| f.id == file_id) {
            if let Ok(ch_id) = rec.channel_id.parse::<u64>() {
                let _ = discord_bot::delete_channel(&st.http, ch_id).await;
            }
        }
    }
    history.retain(|f| f.id != file_id);
    let _ = st.store.save_history(&st.cfg.history_file, &history);
    let _ = std::fs::remove_file(st.thumbnail_dir.join(format!("{file_id}.jpg")));
    Json(json!({ "success": true }))
}

pub async fn rename_file(
    State(st): State<AppState>,
    Path(file_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    let new_name = body["filename"].as_str().unwrap_or("").trim().to_string();
    if new_name.is_empty() { return err(StatusCode::BAD_REQUEST, "Tên không được trống"); }
    let mut history = st.store.load_history(&st.cfg.history_file);
    for f in &mut history { if f.id == file_id { f.filename = new_name; break; } }
    let _ = st.store.save_history(&st.cfg.history_file, &history);
    Json(json!({ "success": true })).into_response()
}

pub async fn move_file(
    State(st): State<AppState>,
    Path(file_id): Path<i64>,
    Json(body): Json<Value>,
) -> Response {
    let target = body.get("folder_id").cloned();
    let folders = st.store.load_folders(&st.cfg.folders_file);
    let folder_name = target.as_ref().and_then(|v| {
        if v.is_null() { return None; }
        let fid = v.as_str().map(|s| s.to_string())
            .or_else(|| v.as_i64().map(|n| n.to_string()))?;
        folders.iter().find(|f| f.id.to_string() == fid).map(|f| f.name.clone())
    });
    let mut history = st.store.load_history(&st.cfg.history_file);
    for f in &mut history {
        if f.id == file_id { f.folder_id = target; f.folder_name = folder_name; break; }
    }
    let _ = st.store.save_history(&st.cfg.history_file, &history);
    Json(json!({ "success": true })).into_response()
}

// ── Stream helpers ─────────────────────────────────────────────────────────────

fn find_record(st: &AppState, file_id: i64) -> Option<FileRecord> {
    st.store.load_history(&st.cfg.history_file).into_iter().find(|f| f.id == file_id)
}

fn make_stream_response(record: FileRecord, st: AppState, inline: bool) -> Response {
    let mime        = mime_for(&record.filename);
    let filename    = record.filename.clone();
    let disposition = if inline {
        format!("inline; filename=\"{filename}\"")
    } else {
        format!("attachment; filename=\"{filename}\"")
    };
    let http     = std::sync::Arc::clone(&st.http);
    let cfg      = std::sync::Arc::clone(&st.cfg);
    let tg_token = st.tg_token.clone();
    let body = Body::from_stream(async_stream::stream! {
        let mut rx = download::merge_to_channel(record, http, cfg, tg_token).await;
        while let Some(chunk) = rx.recv().await {
            yield chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()));
        }
    });
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(body).unwrap()
}

pub async fn merge_file(State(st): State<AppState>, Path(file_id): Path<i64>) -> Response {
    match find_record(&st, file_id) {
        None    => err(StatusCode::NOT_FOUND, "File không tồn tại"),
        Some(r) => make_stream_response(r, st, false),
    }
}

pub async fn preview_file(State(st): State<AppState>, Path(file_id): Path<i64>) -> Response {
    match find_record(&st, file_id) {
        None    => err(StatusCode::NOT_FOUND, "File không tồn tại"),
        Some(r) => make_stream_response(r, st, true),
    }
}

pub async fn thumbnail(State(st): State<AppState>, Path(file_id): Path<i64>) -> Response {
    let record = match find_record(&st, file_id) {
        None    => return err(StatusCode::NOT_FOUND, "File không tồn tại"),
        Some(r) => r,
    };
    let cat = file_category(&record.filename);
    if cat != "image" && cat != "video" {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Không hỗ trợ thumbnail");
    }
    let cache = st.thumbnail_dir.join(format!("{file_id}.jpg"));
    if cache.exists() {
        if let Ok(data) = std::fs::read(&cache) {
            return ([(header::CONTENT_TYPE, "image/jpeg")], data).into_response();
        }
    }
    if record.size_mb > 200.0 && cat == "video" {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Video quá lớn để tạo thumbnail");
    }
    let http     = std::sync::Arc::clone(&st.http);
    let cfg      = std::sync::Arc::clone(&st.cfg);
    let tg_token = st.tg_token.clone();
    let mut rx   = download::merge_to_channel(record, http, cfg, tg_token).await;
    let mut buf  = Vec::new();
    while let Some(chunk) = rx.recv().await {
        match chunk {
            Ok(data) => { buf.extend_from_slice(&data); if buf.len() >= 10*1024*1024 { break; } }
            Err(e)   => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
    match generate_thumbnail(&buf, &cache) {
        Ok(jpeg) => ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response(),
        Err(e)   => err(StatusCode::INTERNAL_SERVER_ERROR, format!("Không thể tạo thumbnail: {e}")),
    }
}

fn generate_thumbnail(buf: &[u8], cache: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    let img   = image::load_from_memory(buf)?;
    let thumb = img.thumbnail(256, 256).to_rgb8();
    let mut out = Vec::new();
    thumb.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)?;
    let _ = std::fs::write(cache, &out);
    Ok(out)
}

// ── Upload ─────────────────────────────────────────────────────────────────────

pub async fn init_upload(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let filename     = body["filename"].as_str().unwrap_or("file").to_string();
    let file_size    = body["file_size"].as_u64().unwrap_or(0);
    let total_chunks = body["total_chunks"].as_u64().unwrap_or(1) as usize;
    let folder_id    = body["folder_id"].as_str().unwrap_or("").to_string();
    let message      = body["message"].as_str().unwrap_or("").to_string();
    let resume_id    = body["session_id"].as_str().unwrap_or("").to_string();

    // Resume check
    if !resume_id.is_empty() {
        let session    = get_session(&st.store, &st.cfg.sessions_file, &resume_id);
        let task_alive = st.sender_map.lock().await.contains_key(&resume_id);
        if let Some(s) = session {
            if s.status == "uploading" && task_alive {
                return Json(json!({
                    "session_id": resume_id,
                    "received_chunks": s.received_chunks,
                    "chunk_size": st.cfg.client_chunk_bytes,
                })).into_response();
            }
        }
        st.sender_map.lock().await.remove(&resume_id);
        delete_session_record(&st.store, &st.cfg.sessions_file, &resume_id);
    }

    // Resolve category
    let (category_id, folder_name) = if !folder_id.is_empty() {
        let folders = st.store.load_folders(&st.cfg.folders_file);
        if let Some(f) = folders.iter().find(|f| f.id.to_string() == folder_id) {
            (Some(serenity::model::id::ChannelId::new(f.discord_category_id as u64)), Some(f.name.clone()))
        } else { (None, None) }
    } else { (None, None) };

    let channel = match discord_bot::get_or_create_channel(&st.http, st.guild_id, &filename, category_id).await {
        Ok(ch) => ch,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let session_id = create_session(
        &st.store, &st.cfg.sessions_file,
        &filename, file_size, total_chunks, &folder_id, &message,
    );
    update_session(&st.store, &st.cfg.sessions_file, &session_id, |s| {
        s.channel_id   = Some(channel.id.get().to_string());
        s.channel_name = Some(channel.name.clone());
        s.folder_name  = folder_name.clone();
    });

    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(64);
    let (result_tx, result_rx) = oneshot::channel();
    let handle = crate::upload::spawn_sender(SenderArgs {
        session_id: session_id.clone(), filename, message, total_chunks,
        channel_id: channel.id,
        http:       std::sync::Arc::clone(&st.http),
        guild_id:   st.guild_id,
        cfg:        std::sync::Arc::clone(&st.cfg),
        tg_enabled: st.tg_enabled,
        tg_token:   st.tg_token.clone(),
        tg_chat_id: st.tg_chat_id.clone(),
        chunk_rx, result_tx,
    });
    st.sender_map.lock().await.insert(session_id.clone(), SenderEntry { chunk_tx, result_rx, handle });

    info!("🚀 Sender task started for session {session_id}");
    Json(json!({
        "session_id": session_id,
        "received_chunks": [],
        "chunk_size": st.cfg.client_chunk_bytes,
    })).into_response()
}

pub async fn upload_chunk(
    State(st): State<AppState>,
    Path((session_id, chunk_index)): Path<(String, usize)>,
    body: Bytes,
) -> Response {
    let session = match get_session(&st.store, &st.cfg.sessions_file, &session_id) {
        None    => return err(StatusCode::NOT_FOUND, "Session không tồn tại"),
        Some(s) => s,
    };
    if session.status != "uploading" && session.status != "sending" {
        return err(StatusCode::BAD_REQUEST, format!("Session status: {}", session.status));
    }
    if body.is_empty() { return err(StatusCode::BAD_REQUEST, "Chunk rỗng"); }

    let sent = {
        let map = st.sender_map.lock().await;
        if let Some(entry) = map.get(&session_id) {
            entry.chunk_tx.try_send((chunk_index, body.clone())).is_ok()
        } else { false }
    };
    if !sent { return err(StatusCode::INTERNAL_SERVER_ERROR, "Sender task không còn hoạt động"); }

    mark_chunk_received(&st.store, &st.cfg.sessions_file, &session_id, chunk_index);
    let received = get_session(&st.store, &st.cfg.sessions_file, &session_id)
        .map(|s| s.received_chunks.len()).unwrap_or(0);
    let total = session.total_chunks;
    info!("  📥 Chunk {}/{} ({:.0}KB)", chunk_index+1, total, body.len() as f64/1024.0);
    Json(json!({ "success": true, "received": received, "total": total })).into_response()
}

pub async fn get_upload_session(State(st): State<AppState>, Path(session_id): Path<String>) -> Response {
    match get_session(&st.store, &st.cfg.sessions_file, &session_id) {
        None    => err(StatusCode::NOT_FOUND, "Session không tồn tại"),
        Some(s) => Json(s).into_response(),
    }
}

pub async fn complete_upload(State(st): State<AppState>, Path(session_id): Path<String>) -> Response {
    let session = match get_session(&st.store, &st.cfg.sessions_file, &session_id) {
        None    => return err(StatusCode::NOT_FOUND, "Session không tồn tại"),
        Some(s) => s,
    };
    if session.received_chunks.len() < session.total_chunks {
        return err(StatusCode::BAD_REQUEST, format!(
            "Chưa đủ chunk: {}/{}", session.received_chunks.len(), session.total_chunks));
    }
    update_session(&st.store, &st.cfg.sessions_file, &session_id, |s| { s.status = "sending".to_string(); });

    let entry = match st.sender_map.lock().await.remove(&session_id) {
        None    => return err(StatusCode::BAD_REQUEST, "Không tìm thấy sender task"),
        Some(e) => e,
    };
    // Drop chunk_tx → signals EOF to receiver
    drop(entry.chunk_tx);

    let result = match entry.result_rx.await {
        Ok(Ok(r))  => r,
        Ok(Err(e)) => {
            delete_session_record(&st.store, &st.cfg.sessions_file, &session_id);
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        Err(_) => {
            delete_session_record(&st.store, &st.cfg.sessions_file, &session_id);
            return err(StatusCode::INTERNAL_SERVER_ERROR, "Sender task bị huỷ");
        }
    };

    let size_mb = (session.file_size as f64 / 1024.0 / 1024.0 * 100.0).round() / 100.0;
    let method_label = match result.method.as_str() {
        "direct" => "Gửi thẳng".to_string(),
        "split"  => format!("Chia {} phần (Discord)", result.parts),
        "dual"   => format!("Chia {} phần (Discord+Telegram)", result.parts),
        _        => format!("Chia {} phần", result.parts),
    };
    let jump_url = result.jump_urls.first().cloned();
    let record = FileRecord {
        id:           current_timestamp_ms(),
        filename:     session.filename.clone(),
        size_mb,
        channel_id:   session.channel_id.clone().unwrap_or_default(),
        channel_name: session.channel_name.clone().unwrap_or_default(),
        folder_id:    if session.folder_id.is_empty() { None }
                      else { Some(Value::String(session.folder_id.clone())) },
        folder_name:  session.folder_name.clone(),
        status:       "sent".to_string(),
        method:       method_label,
        method_key:   result.method.clone(),
        parts:        result.parts,
        parts_info:   result.parts_info.clone(),
        message_ids:  result.message_ids.clone(),
        jump_url,
        sent_at:      current_datetime_display(),
    };
    let mut history = st.store.load_history(&st.cfg.history_file);
    history.insert(0, record.clone());
    let _ = st.store.save_history(&st.cfg.history_file, &history);
    delete_session_record(&st.store, &st.cfg.sessions_file, &session_id);

    info!("✅ Upload complete: {} ({} parts)", session.filename, result.parts);
    Json(json!({ "success": true, "record": record })).into_response()
}

pub async fn cancel_upload(State(st): State<AppState>, Path(session_id): Path<String>) -> impl IntoResponse {
    if let Some(entry) = st.sender_map.lock().await.remove(&session_id) {
        entry.handle.abort();
    }
    delete_session_record(&st.store, &st.cfg.sessions_file, &session_id);
    Json(json!({ "success": true }))
}

// ── Search & Stats ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SearchQuery { q: Option<String> }

pub async fn search_files(State(st): State<AppState>, Query(q): Query<SearchQuery>) -> impl IntoResponse {
    let q_str = q.q.as_deref().unwrap_or("").trim().to_lowercase();
    if q_str.is_empty() { return Json(json!({ "files": [] })); }
    let results: Vec<_> = st.store.load_history(&st.cfg.history_file)
        .into_iter()
        .filter(|f| f.filename.to_lowercase().contains(&q_str))
        .collect();
    Json(json!({ "files": results }))
}

pub async fn get_stats(State(st): State<AppState>) -> impl IntoResponse {
    let history = st.store.load_history(&st.cfg.history_file);
    let folders = st.store.load_folders(&st.cfg.folders_file);
    let total_mb: f64 = history.iter().map(|f| f.size_mb).sum();
    Json(json!({
        "total_files":   history.len(),
        "total_folders": folders.len(),
        "total_mb":      (total_mb * 100.0).round() / 100.0,
    }))
}

// ── Settings ───────────────────────────────────────────────────────────────────

pub async fn get_settings(State(st): State<AppState>) -> impl IntoResponse {
    let cfg_path = st.base_dir.join("config.json");
    let env_path = st.base_dir.join("bot.env");
    let cfg_data: Value = std::fs::read_to_string(&cfg_path)
        .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    let env_data = parse_env(&env_path);
    Json(json!({ "config": cfg_data, "env": env_data }))
}

pub async fn save_settings(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let mut errors = vec![];
    if let Some(cfg_data) = body.get("config") {
        match serde_json::to_string_pretty(cfg_data) {
            Ok(s) => { let _ = std::fs::write(st.base_dir.join("config.json"), s); }
            Err(e) => errors.push(format!("config.json: {e}")),
        }
    }
    if let Some(env_map) = body.get("env").and_then(|v| v.as_object()) {
        let content: String = env_map.iter()
            .map(|(k, v)| format!("{}={}\n", k, v.as_str().unwrap_or("")))
            .collect();
        if let Err(e) = std::fs::write(st.base_dir.join("bot.env"), content) {
            errors.push(format!("bot.env: {e}"));
        }
    }
    if !errors.is_empty() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, errors.join("; "));
    }
    Json(json!({ "success": true, "message": "Đã lưu. Restart app để áp dụng." })).into_response()
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn parse_env(path: &std::path::Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

fn mime_for(filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename).extension()
        .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg"|"jpeg" => "image/jpeg",  "png"  => "image/png",
        "gif"        => "image/gif",   "webp" => "image/webp",
        "svg"        => "image/svg+xml",
        "mp4"        => "video/mp4",   "webm" => "video/webm",
        "mp3"        => "audio/mpeg",  "wav"  => "audio/wav",
        "ogg"        => "audio/ogg",   "pdf"  => "application/pdf",
        "txt"|"md"|"log" => "text/plain",
        "html"|"htm" => "text/html",   "css"  => "text/css",
        "js"         => "application/javascript",
        "json"       => "application/json",
        _            => "application/octet-stream",
    }
}

fn file_category(filename: &str) -> &'static str {
    let ext = std::path::Path::new(filename).extension()
        .and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg"|"jpeg"|"png"|"gif"|"webp"|"bmp"|"tiff"|"svg"|"ico" => "image",
        "mp4"|"webm"|"mkv"|"avi"|"mov"|"wmv"|"flv"|"m4v"         => "video",
        "mp3"|"wav"|"ogg"|"flac"|"aac"|"m4a"|"wma"               => "audio",
        "pdf"                                                      => "pdf",
        _                                                          => "text",
    }
}
