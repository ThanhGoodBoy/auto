/// state.rs — Shared application state passed to every Axum handler.
use serenity::http::Http;
use std::sync::Arc;
use std::path::PathBuf;

use crate::{
    config::Config,
    storage::JsonStore,
    upload::SenderMap,
};

#[derive(Clone)]
pub struct AppState {
    pub cfg:           Arc<Config>,
    pub store:         Arc<JsonStore>,
    pub http:          Arc<Http>,          // Discord HTTP client (from serenity)
    pub guild_id:      serenity::model::id::GuildId,
    pub tg_enabled:    bool,
    pub tg_token:      String,
    pub tg_chat_id:    String,
    pub sender_map:    SenderMap,
    pub base_dir:      PathBuf,
    pub thumbnail_dir: PathBuf,
}
