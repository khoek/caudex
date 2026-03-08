use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::{Builder, NamedTempFile};

use crate::store::tighten_file_permissions;

pub fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    Ok(Builder::new()
        .prefix(prefix)
        .tempdir()
        .context("Failed to create temporary directory")?
        .keep())
}

pub fn create_secure_temp_file(prefix: &str, extension: &str, mode: u32) -> Result<PathBuf> {
    let suffix = if extension.is_empty() {
        String::new()
    } else {
        format!(".{extension}")
    };
    let temp = Builder::new()
        .prefix(prefix)
        .suffix(&suffix)
        .tempfile()
        .context("Failed to create temporary file")?;
    if mode != 0 {
        tighten_file_permissions(temp.path(), mode)?;
    }
    persist_temp_file(temp)
}

pub fn write_toml_file<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let raw = toml::to_string_pretty(value).context("Failed to serialize TOML")?;
    write_bytes(path, raw.as_bytes(), mode)
}

pub fn read_toml_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    toml::from_str(&raw)
        .with_context(|| format!("Failed to parse {}", path.display()))
        .map(Some)
}

pub fn write_bytes(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("Failed to flush {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))?;
    if mode != 0 {
        tighten_file_permissions(path, mode)?;
    }
    Ok(())
}

pub fn cleanup_temp_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

fn persist_temp_file(temp: NamedTempFile) -> Result<PathBuf> {
    let (_file, path) = temp
        .keep()
        .map_err(|err| err.error)
        .context("Failed to persist temporary file")?;
    Ok(path)
}
