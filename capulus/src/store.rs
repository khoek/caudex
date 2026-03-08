use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::NamedTempFile;

#[derive(Debug, Clone)]
pub struct TomlStore<T> {
    path: PathBuf,
    value: T,
}

impl<T: DeserializeOwned> TomlStore<T> {
    pub fn load(path: PathBuf) -> Result<Self> {
        Ok(Self {
            value: load_toml(&path)?,
            path,
        })
    }
}

impl<T: Default + DeserializeOwned> TomlStore<T> {
    pub fn load_or_default(path: PathBuf) -> Result<Self> {
        Ok(Self {
            value: load_toml_or_default(&path)?,
            path,
        })
    }
}

impl<T> TomlStore<T> {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T: Serialize> TomlStore<T> {
    pub fn save(&self, file_mode: Option<u32>, dir_mode: Option<u32>) -> Result<()> {
        write_toml_file(&self.path, &self.value, file_mode, dir_mode)
    }
}

pub fn ensure_directory(path: &Path, mode: Option<u32>) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).with_context(|| format!("Failed to create {}", path.display()))?;
    }
    if let Some(mode) = mode {
        tighten_dir_permissions(path, mode)?;
    }
    Ok(())
}

pub fn tighten_dir_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("Failed to chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub fn tighten_file_permissions(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("Failed to chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub fn atomic_write(
    path: &Path,
    bytes: &[u8],
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent, dir_mode)?;
        let mut temp = NamedTempFile::new_in(parent)
            .with_context(|| format!("Failed to create temporary file in {}", parent.display()))?;
        if let Some(mode) = file_mode {
            tighten_file_permissions(temp.path(), mode)?;
        }
        use std::io::Write;
        temp.write_all(bytes)
            .with_context(|| format!("Failed to write {}", temp.path().display()))?;
        temp.flush()
            .with_context(|| format!("Failed to flush {}", temp.path().display()))?;
        temp.as_file()
            .sync_all()
            .with_context(|| format!("Failed to sync {}", temp.path().display()))?;
        persist_temp_file(temp, path)?;
        if let Some(mode) = file_mode {
            tighten_file_permissions(path, mode)?;
        }
        return Ok(());
    }
    fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    if let Some(mode) = file_mode {
        tighten_file_permissions(path, mode)?;
    }
    Ok(())
}

pub fn write_bytes(
    path: &Path,
    bytes: &[u8],
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent, dir_mode)?;
    }
    fs::write(path, bytes).with_context(|| format!("Failed to write {}", path.display()))?;
    if let Some(mode) = file_mode {
        tighten_file_permissions(path, mode)?;
    }
    Ok(())
}

pub fn load_optional_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    toml::from_str::<T>(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))
        .map(Some)
}

pub fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let content =
        fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str::<T>(&content).with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn load_toml_or_default<T: Default + DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(load_optional_toml(path)?.unwrap_or_default())
}

pub fn write_toml_file<T: Serialize>(
    path: &Path,
    value: &T,
    file_mode: Option<u32>,
    dir_mode: Option<u32>,
) -> Result<()> {
    let content = toml::to_string_pretty(value).context("Failed to serialize config TOML")?;
    atomic_write(path, content.as_bytes(), file_mode, dir_mode)
}

fn persist_temp_file(temp: NamedTempFile, path: &Path) -> Result<()> {
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("Failed to replace {}", path.display()))?;
    }

    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("Failed to move temporary file into {}", path.display()))?;
    Ok(())
}
