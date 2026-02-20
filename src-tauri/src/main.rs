// main.rs — Discord Drive Tauri entry point.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::DefaultBodyLimit,
    http::{header, StatusCode},
    routing::{delete, get, post},
    Router,
};
use serenity::{model::id::GuildId, prelude::*};
use tokio::{sync::{mpsc, Mutex}, time::sleep};
use tower_http::{cors::{Any, CorsLayer}, services::ServeDir};
use tracing::info;

use discord_drive_lib::{
    api,
    config::Config,
    discord_bot::Handler,
    state::AppState,
    storage::JsonStore,
    upload::new_sender_map,
};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let base_dir = if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        PathBuf::from(&manifest)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(&manifest))
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
    };
    info!("📂 base_dir = {}", base_dir.display());

    let env_path = base_dir.join("bot.env");
    if env_path.exists() {
        dotenvy::from_path(&env_path).ok();
    } else {
        dotenvy::dotenv().ok();
    }

    let discord_token = std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set in bot.env");
    let guild_id_raw: u64 = std::env::var("DISCORD_GUILD_ID")
        .expect("DISCORD_GUILD_ID not set")
        .parse()
        .expect("DISCORD_GUILD_ID must be a number");
    let guild_id = GuildId::new(guild_id_raw);

    let tg_token   = std::env::var("TELEGRAM_TOKEN").unwrap_or_default();
    let tg_chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
    let tg_enabled = !tg_token.is_empty() && !tg_chat_id.is_empty();

    if tg_enabled {
        info!("✅ Telegram enabled — dual-platform upload active");
    } else {
        info!("ℹ️  Telegram not configured — Discord only");
    }

    let cfg = Arc::new(Config::load(&base_dir));
    cfg.print_summary();

    // ── FIX: chunk upload limit = client_chunk_mb * parallel_chunks + 20% headroom ──
    // Use 500MB hard cap; individual route overrides the global 2MB Axum default.
    let chunk_body_limit = ((cfg.client_chunk_bytes as f64) * 1.2) as usize;
    let chunk_body_limit = chunk_body_limit.max(50 * 1024 * 1024); // minimum 50MB
    info!("📦 Chunk body limit: {:.0}MB", chunk_body_limit as f64 / 1024.0 / 1024.0);

    let thumbnail_dir = base_dir.join("thumbnails_cache");
    std::fs::create_dir_all(&thumbnail_dir).ok();

    let store = Arc::new(JsonStore::new(base_dir.clone()));

    // ── Discord bot ────────────────────────────────────────────────────────────
    info!("🤖 Starting Discord bot...");
    let (ready_tx, mut ready_rx) = mpsc::channel::<()>(1);

    let handler = Handler {
        guild_id,
        history_file: cfg.history_file.clone(),
        folders_file: cfg.folders_file.clone(),
        store:        Arc::clone(&store),
        ready_tx:     Mutex::new(Some(ready_tx)),
    };

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&discord_token, intents)
        .event_handler(handler)
        .await
        .expect("Failed to create Discord client");

    let http = Arc::clone(&client.http);

    tokio::spawn(async move {
        if let Err(e) = client.start().await {
            eprintln!("❌ Discord client error: {e}");
        }
    });

    match tokio::time::timeout(Duration::from_secs(30), ready_rx.recv()).await {
        Ok(Some(())) => info!("✅ Discord bot ready"),
        _ => {
            eprintln!("❌ Discord bot did not become ready within 30s. Check DISCORD_TOKEN.");
            std::process::exit(1);
        }
    }

    // ── AppState ───────────────────────────────────────────────────────────────
    let app_state = AppState {
        cfg:          Arc::clone(&cfg),
        store:        Arc::clone(&store),
        http:         Arc::clone(&http),
        guild_id,
        tg_enabled,
        tg_token:     tg_token.clone(),
        tg_chat_id:   tg_chat_id.clone(),
        sender_map:   new_sender_map(),
        base_dir:     base_dir.clone(),
        thumbnail_dir: thumbnail_dir.clone(),
    };

    // ── Axum router ────────────────────────────────────────────────────────────
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);
    let static_dir = base_dir.join("static");
    let static_dir_root = static_dir.clone();

    let router = Router::new()
        .route("/api/health",                 get(api::health))
        .route("/api/folders",                get(api::get_folders).post(api::create_folder))
        .route("/api/folders/:id",            delete(api::delete_folder))
        .route("/api/files",                  get(api::get_files))
        .route("/api/files/:id",              delete(api::delete_file).patch(api::rename_file))
        .route("/api/files/:id/move",         post(api::move_file))
        .route("/api/merge/:id",              get(api::merge_file))
        .route("/api/preview/:id",            get(api::preview_file))
        .route("/api/thumbnail/:id",          get(api::thumbnail))
        .route("/api/upload/init",            post(api::init_upload))
        // ── FIX: override Axum's 2MB default body limit for chunk uploads ──────
        .route("/api/upload/chunk/:sid/:idx",
            post(api::upload_chunk)
                .layer(DefaultBodyLimit::max(chunk_body_limit)))
        // ──────────────────────────────────────────────────────────────────────
        .route("/api/upload/session/:sid",    get(api::get_upload_session).delete(api::cancel_upload))
        .route("/api/upload/complete/:sid",   post(api::complete_upload))
        .route("/api/search",                 get(api::search_files))
        .route("/api/stats",                  get(api::get_stats))
        .route("/api/settings",               get(api::get_settings).post(api::save_settings))
        .route("/", get(|| async move {
            let path = static_dir_root.join("index.html");
            match tokio::fs::read(&path).await {
                Ok(bytes) => axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(axum::body::Body::from(bytes))
                    .unwrap(),
                Err(_) => axum::response::Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(axum::body::Body::from("index.html not found"))
                    .unwrap(),
            }
        }))
        .nest_service("/static", ServeDir::new(&static_dir))
        .fallback_service(ServeDir::new(&static_dir).append_index_html_on_directories(true))
        .with_state(app_state.clone())
        .layer(cors);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind {addr}: {e}"));
    info!("🌐 HTTP server listening on http://{addr}");

    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("axum server error");
    });

    // GC task
    {
        let store2 = Arc::clone(&store);
        let cfg2   = Arc::clone(&cfg);
        tokio::spawn(async move { gc_task(store2, cfg2).await; });
    }

    // ── Tauri window ───────────────────────────────────────────────────────────
    info!("🖥️  Opening window → http://127.0.0.1:{}", cfg.port);

    tauri::Builder::default()
        .setup(|_app| Ok(()))
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn gc_task(store: Arc<JsonStore>, cfg: Arc<Config>) {
    loop {
        sleep(Duration::from_secs(cfg.gc_interval_s)).await;
        let sessions = store.load_sessions(&cfg.sessions_file);
        let now      = chrono::Utc::now().timestamp() as u64;
        let mut expired: Vec<String> = vec![];
        for (sid, session) in &sessions {
            if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&session.created_at) {
                let age = now.saturating_sub(created.timestamp() as u64);
                if age > cfg.session_ttl_s && session.status == "uploading" {
                    expired.push(sid.clone());
                }
            }
        }
        if !expired.is_empty() {
            let mut sessions = sessions;
            for sid in &expired {
                sessions.remove(sid);
                info!("🧹 GC: session {sid} expired → removed");
            }
            let _ = store.save_sessions(&cfg.sessions_file, &sessions);
        }
    }
}
