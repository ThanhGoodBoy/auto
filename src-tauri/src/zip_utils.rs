/// zip_utils.rs — ZIP pack/unpack helpers.
use anyhow::{Context, Result};
use std::io::{Cursor, Read, Write};
use zip::{write::FileOptions, CompressionMethod, ZipArchive, ZipWriter};

/// Pack `data` into a ZIP archive containing a single entry named `entry_name`.
pub fn zip_bytes(data: &[u8], entry_name: &str, compress_level: u32) -> Result<Vec<u8>> {
    let buf = Vec::with_capacity(data.len() + 512);
    let cursor = Cursor::new(buf);
    let mut zip = ZipWriter::new(cursor);

    let method = if compress_level == 0 {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    };

    let opts: FileOptions<()> = FileOptions::default()
        .compression_method(method)
        .compression_level(if compress_level == 0 { None } else { Some(compress_level as i64) });

    zip.start_file(entry_name, opts)?;
    zip.write_all(data)?;
    let cursor = zip.finish()?;
    Ok(cursor.into_inner())
}

/// Unpack a ZIP archive and return the first entry's bytes.
/// If `data` is not a ZIP, returns it unchanged (backward compat).
pub fn unzip_or_raw(data: Vec<u8>) -> Result<Vec<u8>> {
    // PK magic
    if data.len() < 4 || &data[..4] != b"PK\x03\x04" {
        return Ok(data);
    }
    let cursor = Cursor::new(&data);
    let mut archive = ZipArchive::new(cursor).context("open zip")?;
    let mut entry = archive.by_index(0).context("read zip entry")?;
    let mut out = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut out).context("read zip entry data")?;
    Ok(out)
}
