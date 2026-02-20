/// discord_bot.rs — Discord bot using Serenity.
use anyhow::{anyhow, Context as AnyhowContext, Result};
use serenity::{
    async_trait,
    http::Http,
    model::{
        channel::GuildChannel,
        gateway::Ready,
        id::{ChannelId, GuildId},
    },
    prelude::*,
};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

use crate::storage::JsonStore;

pub struct Handler {
    pub guild_id:      GuildId,
    pub history_file:  String,
    pub folders_file:  String,
    pub store:         Arc<JsonStore>,
    pub ready_tx:      Mutex<Option<mpsc::Sender<()>>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: serenity::prelude::Context, ready: Ready) {
        info!("✅ Bot online: {}", ready.user.name);
        if let Some(tx) = self.ready_tx.lock().await.take() {
            let _ = tx.send(()).await;
        }
    }

    async fn channel_delete(
        &self,
        _ctx: serenity::prelude::Context,
        channel: GuildChannel,
        _messages: Option<Vec<serenity::model::channel::Message>>,
    ) {
        let mut history = self.store.load_history(&self.history_file);
        let before = history.len();
        history.retain(|f| f.channel_id != channel.id.get().to_string());
        if history.len() < before {
            if let Err(e) = self.store.save_history(&self.history_file, &history) {
                error!("Failed to save history after channel delete: {e}");
            }
            info!("🗑️ Channel #{} deleted → removed from history", channel.name);
        }
    }

    async fn category_delete(
        &self,
        _ctx: serenity::prelude::Context,
        category: GuildChannel,
    ) {
        let mut folders = self.store.load_folders(&self.folders_file);
        let before = folders.len();
        folders.retain(|f| f.discord_category_id != category.id.get() as i64);
        if folders.len() < before {
            if let Err(e) = self.store.save_folders(&self.folders_file, &folders) {
                error!("Failed to save folders after category delete: {e}");
            }
            info!("🗑️ Category {} deleted → removed from folders", category.name);
        }
    }
}

pub fn sanitize_name(name: &str) -> String {
    use std::path::Path;
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let lower = stem.to_lowercase();
    let filtered: String = lower.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .collect();
    let dashed = filtered.trim().replace(' ', "-");
    let mut result = String::new();
    let mut last_dash = false;
    for ch in dashed.chars() {
        if ch == '-' {
            if !last_dash { result.push('-'); }
            last_dash = true;
        } else {
            result.push(ch);
            last_dash = false;
        }
    }
    let trimmed = result.trim_matches('-').to_string();
    if trimmed.is_empty() { "file".to_string() } else { trimmed.chars().take(100).collect() }
}

pub async fn get_or_create_category(
    http:     &Arc<Http>,
    guild_id: GuildId,
    name:     &str,
) -> Result<GuildChannel> {
    let safe = sanitize_name(name);
    let guild = guild_id.to_partial_guild(http).await
        .context("fetch guild")?;
    let channels = guild.channels(http).await.context("fetch channels")?;
    for (_, ch) in &channels {
        if ch.kind == serenity::model::channel::ChannelType::Category
            && ch.name.to_lowercase() == safe
        {
            return Ok(ch.clone());
        }
    }
    let cat = guild.create_channel(
        http,
        serenity::builder::CreateChannel::new(&safe)
            .kind(serenity::model::channel::ChannelType::Category),
    ).await.context("create category")?;
    info!("📁 Created category: {safe}");
    Ok(cat)
}

pub async fn get_or_create_channel(
    http:        &Arc<Http>,
    guild_id:    GuildId,
    file_name:   &str,
    category_id: Option<ChannelId>,
) -> Result<GuildChannel> {
    let safe = sanitize_name(file_name);
    let guild = guild_id.to_partial_guild(http).await
        .context("fetch guild")?;
    let channels = guild.channels(http).await.context("fetch channels")?;
    for (_, ch) in &channels {
        if ch.kind == serenity::model::channel::ChannelType::Text
            && ch.name.to_lowercase() == safe
            && (category_id.is_none() || ch.parent_id == category_id)
        {
            return Ok(ch.clone());
        }
    }
    let mut builder = serenity::builder::CreateChannel::new(&safe)
        .kind(serenity::model::channel::ChannelType::Text);
    if let Some(cat_id) = category_id {
        builder = builder.category(cat_id);
    }
    let ch = guild.create_channel(http, builder).await.context("create channel")?;
    info!("📄 Created channel: {safe}");
    Ok(ch)
}

pub async fn delete_channel(http: &Arc<Http>, channel_id: u64) -> Result<()> {
    ChannelId::new(channel_id).delete(http).await.context("delete channel")?;
    Ok(())
}

pub async fn delete_category(http: &Arc<Http>, guild_id: GuildId, category_id: u64) -> Result<()> {
    let guild = guild_id.to_partial_guild(http).await.context("fetch guild")?;
    let channels = guild.channels(http).await.context("fetch channels")?;
    let cat_id = ChannelId::new(category_id);
    let has_children = channels.values().any(|c| c.parent_id == Some(cat_id));
    if !has_children {
        cat_id.delete(http).await.context("delete category")?;
    }
    Ok(())
}

pub async fn send_part(
    http:       &Arc<Http>,
    channel_id: ChannelId,
    zip_bytes:  Vec<u8>,
    zip_name:   String,
    content:    String,
) -> Result<(i64, String)> {
    let attachment = serenity::builder::CreateAttachment::bytes(zip_bytes, &zip_name);
    let builder = serenity::builder::CreateMessage::new()
        .content(&content)
        .add_file(attachment);
    let msg = channel_id.send_message(http, builder).await
        .context("send Discord message")?;
    Ok((msg.id.get() as i64, msg.link()))
}

pub async fn fetch_attachment_url(
    http:       &Arc<Http>,
    channel_id: u64,
    message_id: u64,
) -> Result<String> {
    let msg = ChannelId::new(channel_id)
        .message(http, message_id).await
        .context("fetch message")?;
    let att = msg.attachments.into_iter().next()
        .ok_or_else(|| anyhow!("No attachment on message {message_id}"))?;
    Ok(att.url)
}
