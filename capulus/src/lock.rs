use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use fs2::FileExt;

const BYPASS_ENV_VAR: &str = "CAPULUS_SINGLE_INSTANCE_BYPASS";

static ACTIVE_BYPASS_TOKEN: OnceLock<OsString> = OnceLock::new();

#[derive(Debug)]
pub struct InvocationLock {
    _file: Option<File>,
    path: PathBuf,
}

impl InvocationLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another `{tool}` invocation is already running (lockfile: {path})")]
    AlreadyRunning { tool: String, path: String },

    #[error("failed to create lock directory for `{tool}` at {path}: {source}")]
    CreateDir {
        tool: String,
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to open lockfile for `{tool}` at {path}: {source}")]
    Open {
        tool: String,
        path: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to update lockfile for `{tool}` at {path}: {source}")]
    Update {
        tool: String,
        path: String,
        #[source]
        source: io::Error,
    },
}

pub fn acquire(tool: &str) -> Result<InvocationLock, LockError> {
    let tool = sanitize_component(tool);
    let token = bypass_token(&tool);
    let _ = ACTIVE_BYPASS_TOKEN.set(token.clone());
    let path = lock_path(&tool);
    if env::var_os(BYPASS_ENV_VAR).as_ref() == Some(&token) {
        return Ok(InvocationLock { _file: None, path });
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LockError::CreateDir {
            tool: tool.clone(),
            path: parent.display().to_string(),
            source,
        })?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| LockError::Open {
            tool: tool.clone(),
            path: path.display().to_string(),
            source,
        })?;

    match file.try_lock_exclusive() {
        Ok(()) => write_lock_metadata(&tool, &path, &mut file)?,
        Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
            return Err(LockError::AlreadyRunning {
                tool,
                path: path.display().to_string(),
            });
        }
        Err(source) => {
            return Err(LockError::Open {
                tool,
                path: path.display().to_string(),
                source,
            });
        }
    }

    Ok(InvocationLock {
        _file: Some(file),
        path,
    })
}

pub fn configure_child_command(command: &mut Command) {
    if let Some(token) = ACTIVE_BYPASS_TOKEN.get() {
        command.env(BYPASS_ENV_VAR, token);
    }
}

pub fn configure_privileged_child_command(command: &mut Command, program: &str) {
    if ACTIVE_BYPASS_TOKEN.get().is_none() {
        return;
    }
    match program {
        "doas" => {
            command.arg("-E");
        }
        "sudo" => {
            command.arg(format!("--preserve-env={BYPASS_ENV_VAR}"));
        }
        _ => {}
    }
    configure_child_command(command);
}

fn write_lock_metadata(tool: &str, path: &Path, file: &mut File) -> Result<(), LockError> {
    file.set_len(0).map_err(|source| LockError::Update {
        tool: tool.to_owned(),
        path: path.display().to_string(),
        source,
    })?;
    file.seek(SeekFrom::Start(0))
        .map_err(|source| LockError::Update {
            tool: tool.to_owned(),
            path: path.display().to_string(),
            source,
        })?;
    writeln!(file, "{}", std::process::id()).map_err(|source| LockError::Update {
        tool: tool.to_owned(),
        path: path.display().to_string(),
        source,
    })?;
    file.sync_data().map_err(|source| LockError::Update {
        tool: tool.to_owned(),
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

fn lock_path(tool: &str) -> PathBuf {
    lock_root().join(format!("{tool}.lock"))
}

fn lock_root() -> PathBuf {
    if let Some(path) = nonempty_env_var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join("capulus");
    }
    if let Some(path) = nonempty_env_var_os("HOME").or_else(|| nonempty_env_var_os("USERPROFILE")) {
        return PathBuf::from(path).join(".capulus").join("locks");
    }
    env::temp_dir().join("capulus")
}

fn nonempty_env_var_os(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn bypass_token(tool: &str) -> OsString {
    OsString::from(format!("capulus:{tool}"))
}

fn sanitize_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "tool".to_owned()
    } else {
        output
    }
}
