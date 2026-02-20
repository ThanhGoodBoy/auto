/// config.rs — Discord Drive Config Loader
/// Mirrors Python config.py: reads config.json, validates, falls back to defaults.
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

// ─── Raw JSON shapes (with optional fields for validation) ────────────────────

#[derive(Deserialize, Default, Clone)]
struct RawUpload {
    client_chunk_mb:            Option<u64>,
    parallel_chunks:            Option<usize>,
    discord_safe_ratio:         Option<f64>,
    zip_compress_level:         Option<u32>,
    discord_parallel_sends:     Option<usize>,
    tg_parallel_sends:          Option<usize>,
    discord_send_retries:       Option<u32>,
    discord_retry_base_delay_s: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct RawDownload {
    http_timeout_s:          Option<u64>,
    retry_count:             Option<u32>,
    retry_base_delay_s:      Option<u64>,
    part_delay_ms:           Option<u64>,
    stream_buffer_kb:        Option<usize>,
    large_file_threshold_mb: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct RawRam {
    max_total_upload_mb: Option<u64>,
    session_ttl_minutes: Option<u64>,
    gc_interval_minutes: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct RawServer {
    host:            Option<String>,
    port:            Option<u16>,
    log_level:       Option<String>,
    keep_alive_s:    Option<u64>,
    max_concurrency: Option<usize>,
}

#[derive(Deserialize, Default, Clone)]
struct RawData {
    history_file:  Option<String>,
    folders_file:  Option<String>,
    sessions_file: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
struct RawTelegram {
    file_limit_mb: Option<u64>,
}

#[derive(Deserialize, Default, Clone)]
struct RawConfig {
    #[serde(default)]
    upload:   RawUpload,
    #[serde(default)]
    download: RawDownload,
    #[serde(default)]
    ram:      RawRam,
    #[serde(default)]
    server:   RawServer,
    #[serde(default)]
    data:     RawData,
    #[serde(default)]
    telegram: RawTelegram,
}

// ─── Validated, exported config ───────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    // Upload
    pub client_chunk_bytes:     u64,     // MB → bytes
    pub parallel_chunks:        usize,
    pub discord_safe_ratio:     f64,
    pub zip_compress_level:     u32,
    pub discord_parallel_sends: usize,
    pub tg_parallel_sends:      usize,
    pub discord_send_retries:   u32,
    pub discord_retry_base_s:   u64,

    // Download
    pub http_timeout_s:          u64,
    pub download_retry:          u32,
    pub download_retry_base_s:   u64,
    pub part_delay_ms:           u64,
    pub read_buffer_bytes:       usize,  // KB → bytes
    pub large_file_threshold_mb: u64,

    // RAM
    pub max_upload_ram_bytes: u64,       // MB → bytes (0 = unlimited)
    pub session_ttl_s:        u64,       // minutes → seconds
    pub gc_interval_s:        u64,       // minutes → seconds

    // Server
    pub host:            String,
    pub port:            u16,
    pub log_level:       String,
    pub keep_alive_s:    u64,
    pub max_concurrency: usize,

    // Data files
    pub history_file:  String,
    pub folders_file:  String,
    pub sessions_file: String,

    // Telegram
    pub tg_file_limit_bytes: u64,        // MB → bytes
}

impl Config {
    pub fn load(base_dir: &PathBuf) -> Self {
        let path = base_dir.join("config.json");
        let raw: RawConfig = if path.exists() {
            match fs::read_to_string(&path)
                .context("read config.json")
                .and_then(|s| {
                    // Strip keys starting with "_" using serde_json value manipulation
                    let mut val: serde_json::Value = serde_json::from_str(&s)?;
                    strip_comment_keys(&mut val);
                    serde_json::from_value(val).map_err(Into::into)
                }) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("⚠️  config.json parse error: {e} → using defaults");
                    RawConfig::default()
                }
            }
        } else {
            eprintln!("⚠️  config.json not found → using defaults");
            RawConfig::default()
        };

        Self::from_raw(raw)
    }

    fn from_raw(r: RawConfig) -> Self {
        let u = &r.upload;
        let d = &r.download;
        let m = &r.ram;
        let s = &r.server;
        let dt = &r.data;
        let tg = &r.telegram;

        macro_rules! clamp {
            ($val:expr, $default:expr, $lo:expr, $hi:expr) => {{
                let v = $val.unwrap_or($default);
                let lo = $lo;
                let hi = $hi;
                if v < lo || v > hi {
                    eprintln!("⚠️  config value {} out of range [{lo},{hi}] → default {}", v, $default);
                    $default
                } else {
                    v
                }
            }};
        }
        macro_rules! clamp_opt_hi {
            ($val:expr, $default:expr, $lo:expr) => {{
                let v = $val.unwrap_or($default);
                if v < $lo {
                    eprintln!("⚠️  config value {} < min {} → default {}", v, $lo, $default);
                    $default
                } else {
                    v
                }
            }};
        }

        let client_chunk_mb = clamp!(u.client_chunk_mb, 4, 1, 50);
        let parallel_chunks = clamp!(u.parallel_chunks, 4, 1, 16);
        let discord_safe_ratio_raw = u.discord_safe_ratio.unwrap_or(0.85_f64);
        let discord_safe_ratio = if !(0.5..=0.99).contains(&discord_safe_ratio_raw) { 0.85 } else { discord_safe_ratio_raw };
        let zip_compress_level = clamp!(u.zip_compress_level, 0, 0, 9);
        let discord_parallel_sends = clamp!(u.discord_parallel_sends, 3, 1, 5);
        let tg_parallel_sends = clamp!(u.tg_parallel_sends, 3, 1, 5);
        let discord_send_retries = clamp!(u.discord_send_retries, 3, 1, 10);
        let discord_retry_base_s = clamp!(u.discord_retry_base_delay_s, 2, 1, 30);

        let http_timeout_s = clamp!(d.http_timeout_s, 600, 30, 3600);
        let download_retry = clamp!(d.retry_count, 3, 1, 10);
        let download_retry_base_s = clamp!(d.retry_base_delay_s, 2, 1, 30);
        let part_delay_ms = clamp!(d.part_delay_ms, 150, 0, 5000);
        let stream_buffer_kb = clamp!(d.stream_buffer_kb, 64, 8, 4096);
        let large_file_threshold_mb = clamp_opt_hi!(d.large_file_threshold_mb, 500, 50);

        let max_total_upload_mb = m.max_total_upload_mb.unwrap_or(512);
        let session_ttl_minutes = clamp!(m.session_ttl_minutes, 60, 5, 1440);
        let gc_interval_minutes = clamp!(m.gc_interval_minutes, 10, 1, 120);

        let log_level_raw = s.log_level.clone().unwrap_or_else(|| "info".to_string());
        let log_level = if ["debug","info","warning","error","critical"].contains(&log_level_raw.as_str()) {
            log_level_raw
        } else { "info".to_string() };

        let tg_file_limit_mb = clamp!(tg.file_limit_mb, 50, 10, 4000);

        Config {
            client_chunk_bytes:       client_chunk_mb * 1024 * 1024,
            parallel_chunks,
            discord_safe_ratio,
            zip_compress_level,
            discord_parallel_sends,
            tg_parallel_sends,
            discord_send_retries,
            discord_retry_base_s,

            http_timeout_s,
            download_retry,
            download_retry_base_s,
            part_delay_ms,
            read_buffer_bytes:       stream_buffer_kb * 1024,
            large_file_threshold_mb,

            max_upload_ram_bytes: max_total_upload_mb * 1024 * 1024,
            session_ttl_s:        session_ttl_minutes * 60,
            gc_interval_s:        gc_interval_minutes * 60,

            host:            s.host.clone().unwrap_or_else(|| "0.0.0.0".to_string()),
            port:            s.port.unwrap_or(8000),
            log_level,
            keep_alive_s:    clamp!(s.keep_alive_s, 600, 10, 3600),
            max_concurrency: clamp!(s.max_concurrency, 5, 1, 100),

            history_file:  dt.history_file.clone().unwrap_or_else(|| "file_history.json".to_string()),
            folders_file:  dt.folders_file.clone().unwrap_or_else(|| "folders.json".to_string()),
            sessions_file: dt.sessions_file.clone().unwrap_or_else(|| "upload_sessions.json".to_string()),

            tg_file_limit_bytes: tg_file_limit_mb * 1024 * 1024,
        }
    }

    pub fn print_summary(&self) {
        println!("{}", "─".repeat(60));
        println!("⚙️  Discord Drive Config (Rust + Tauri)");
        let chunk_mb = self.client_chunk_bytes / 1024 / 1024;
        println!("   Upload  : chunk={chunk_mb}MB  parallel_chunks={}  safe_ratio={}", self.parallel_chunks, self.discord_safe_ratio);
        println!("   Discord : parallel_sends={}  zip_level={}  retries={}", self.discord_parallel_sends, self.zip_compress_level, self.discord_send_retries);
        let tg_limit_mb = self.tg_file_limit_bytes / 1024 / 1024;
        println!("   Telegram: parallel_sends={}  file_limit={tg_limit_mb}MB", self.tg_parallel_sends);
        println!("   Download: timeout={}s  retry={}  large>={}MB", self.http_timeout_s, self.download_retry, self.large_file_threshold_mb);
        let ram_limit_mb = self.max_upload_ram_bytes / 1024 / 1024;
        let ram_label = if self.max_upload_ram_bytes == 0 { "unlimited".to_string() } else { format!("{ram_limit_mb}MB") };
        println!("   RAM     : max_upload={ram_label}  ttl={}min  gc={}min", self.session_ttl_s / 60, self.gc_interval_s / 60);
        println!("   Server  : {}:{}  log={}  concurrency={}", self.host, self.port, self.log_level, self.max_concurrency);
        println!("{}", "─".repeat(60));
    }
}

fn strip_comment_keys(val: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = val {
        let keys_to_remove: Vec<String> = map.keys()
            .filter(|k| k.starts_with('_'))
            .cloned()
            .collect();
        for k in keys_to_remove {
            map.remove(&k);
        }
        for v in map.values_mut() {
            strip_comment_keys(v);
        }
    }
}
