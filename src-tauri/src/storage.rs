/// storage.rs — JSON persistence helpers.
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id:                  i64,
    pub name:                String,
    pub discord_category_id: i64,
    pub created_at:          String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartInfo {
    pub part:       u32,
    pub platform:   String,
    pub message_id: i64,
    pub channel_id: Option<String>,
    pub file_id:    Option<String>,
    pub jump_url:   Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub id:           i64,
    pub filename:     String,
    pub size_mb:      f64,
    pub channel_id:   String,
    pub channel_name: String,
    pub folder_id:    Option<Value>,
    pub folder_name:  Option<String>,
    pub status:       String,
    pub method:       String,
    pub method_key:   String,
    pub parts:        u32,
    pub parts_info:   Vec<PartInfo>,
    pub message_ids:  Vec<i64>,
    pub jump_url:     Option<String>,
    pub sent_at:      String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSession {
    pub session_id:      String,
    pub filename:        String,
    pub file_size:       u64,
    pub total_chunks:    usize,
    pub received_chunks: Vec<usize>,
    pub folder_id:       String,
    pub message:         String,
    pub status:          String,
    pub created_at:      String,
    pub channel_id:      Option<String>,
    pub channel_name:    Option<String>,
    pub folder_name:     Option<String>,
    pub discord_result:  Option<Value>,
}

pub struct JsonStore {
    pub base_dir: PathBuf,
}

impl JsonStore {
    pub fn new(base_dir: PathBuf) -> Self { Self { base_dir } }

    fn path(&self, filename: &str) -> PathBuf { self.base_dir.join(filename) }

    pub fn load_json<T: for<'de> Deserialize<'de> + Default>(&self, filename: &str) -> T {
        let path = self.path(filename);
        if !path.exists() { return T::default(); }
        match fs::read_to_string(&path).and_then(|s|
            serde_json::from_str(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        ) {
            Ok(v)  => v,
            Err(e) => { eprintln!("⚠️  Failed to load {filename}: {e}"); T::default() }
        }
    }

    pub fn save_json<T: Serialize + ?Sized>(&self, filename: &str, data: &T) -> Result<()> {
        let path = self.path(filename);
        let json = serde_json::to_string_pretty(data)?;
        fs::write(&path, json).context(format!("write {filename}"))?;
        Ok(())
    }

    pub fn load_folders(&self, file: &str) -> Vec<Folder> { self.load_json(file) }
    pub fn save_folders(&self, file: &str, folders: &[Folder]) -> Result<()> { self.save_json(file, folders) }

    pub fn load_history(&self, file: &str) -> Vec<FileRecord> { self.load_json(file) }
    pub fn save_history(&self, file: &str, records: &[FileRecord]) -> Result<()> { self.save_json(file, records) }

    pub fn load_sessions(&self, file: &str) -> HashMap<String, UploadSession> { self.load_json(file) }
    pub fn save_sessions(&self, file: &str, sessions: &HashMap<String, UploadSession>) -> Result<()> {
        self.save_json(file, sessions)
    }
}

pub fn current_timestamp_ms() -> i64 { Utc::now().timestamp_millis() }
pub fn current_datetime_display() -> String { Local::now().format("%d/%m/%Y %H:%M").to_string() }
pub fn current_datetime_iso() -> String { Utc::now().to_rfc3339() }
