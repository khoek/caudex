use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use fs2::FileExt;
use indicatif::ProgressBar;

use crate::ui::{print_notice, spinner, stderr_is_interactive};

const BYPASS_ENV_VAR: &str = "CAPULUS_SINGLE_INSTANCE_BYPASS";
const LOCK_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

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

pub fn acquire_in(
    root: impl AsRef<Path>,
    tool: &str,
    wait: bool,
) -> Result<InvocationLock, LockError> {
    let tool = sanitize_component(tool);
    let token = bypass_token(&tool);
    let _ = ACTIVE_BYPASS_TOKEN.set(token.clone());
    if env::var_os(BYPASS_ENV_VAR).as_ref() == Some(&token) {
        let path = lock_path(root.as_ref(), &tool);
        return Ok(InvocationLock { _file: None, path });
    }
    acquire_sanitized(root.as_ref(), &tool, wait)
}

pub fn acquire_named_in(
    root: impl AsRef<Path>,
    name: &str,
    wait: bool,
) -> Result<InvocationLock, LockError> {
    acquire_sanitized(root.as_ref(), &sanitize_component(name), wait)
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

fn acquire_sanitized(root: &Path, tool: &str, wait: bool) -> Result<InvocationLock, LockError> {
    let path = lock_path(root, tool);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LockError::CreateDir {
            tool: tool.to_owned(),
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
            tool: tool.to_owned(),
            path: path.display().to_string(),
            source,
        })?;

    let mut wait_ui = LockWaitUi::new(tool, &path);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => {
                let result = write_lock_metadata(tool, &path, &mut file);
                wait_ui.clear();
                result?;
                break;
            }
            Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                if !wait {
                    return Err(LockError::AlreadyRunning {
                        tool: tool.to_owned(),
                        path: path.display().to_string(),
                    });
                }
                wait_ui.show();
                thread::sleep(LOCK_WAIT_POLL_INTERVAL);
            }
            Err(source) => {
                wait_ui.clear();
                return Err(LockError::Open {
                    tool: tool.to_owned(),
                    path: path.display().to_string(),
                    source,
                });
            }
        }
    }

    Ok(InvocationLock {
        _file: Some(file),
        path,
    })
}

fn lock_path(root: &Path, tool: &str) -> PathBuf {
    root.join(format!("{tool}.lock"))
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

struct LockWaitUi {
    message: String,
    progress: Option<ProgressBar>,
    notice_printed: bool,
}

impl LockWaitUi {
    fn new(tool: &str, path: &Path) -> Self {
        Self {
            message: format!(
                "Waiting for another `{tool}` invocation to finish ({})",
                path.display()
            ),
            progress: None,
            notice_printed: false,
        }
    }

    fn show(&mut self) {
        if let Some(progress) = &self.progress {
            progress.tick();
            return;
        }
        if stderr_is_interactive() {
            self.progress = Some(spinner(&self.message));
            return;
        }
        if !self.notice_printed {
            print_notice(&self.message);
            self.notice_printed = true;
        }
    }

    fn clear(&mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{LockError, acquire_in, acquire_named_in};

    fn unique_tool_name() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time after unix epoch")
            .as_nanos();
        format!("capulus-lock-test-{}-{now}", std::process::id())
    }

    #[test]
    fn acquire_without_wait_returns_already_running() {
        let temp = tempfile::TempDir::new().expect("temp lock root");
        let root = temp.path().join("locks");
        let tool = unique_tool_name();
        let first = acquire_in(&root, &tool, false).expect("acquire first lock");
        let first_path = first.path().to_path_buf();
        let second = acquire_in(&root, &tool, false).expect_err("second acquisition should fail");
        assert!(matches!(second, LockError::AlreadyRunning { .. }));
        drop(first);
        fs::remove_file(first_path).ok();
    }

    #[test]
    fn acquire_with_wait_blocks_until_previous_holder_releases() {
        let temp = tempfile::TempDir::new().expect("temp lock root");
        let root = temp.path().join("locks");
        let tool = unique_tool_name();
        let first = acquire_in(&root, &tool, false).expect("acquire first lock");
        let (done_tx, done_rx) = mpsc::channel();
        let waiting_tool = tool.clone();
        let waiting_root = root.clone();

        let handle = std::thread::spawn(move || {
            let second = acquire_in(&waiting_root, &waiting_tool, true)
                .expect("waiting acquisition succeeds");
            done_tx
                .send(second.path().to_path_buf())
                .expect("send path");
            second
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "waiting acquisition should still be blocked"
        );
        drop(first);

        let second_path = done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("waiting acquisition completes");
        let second = handle.join().expect("join waiter");
        drop(second);
        fs::remove_file(second_path).ok();
    }

    #[test]
    fn acquire_named_uses_the_same_lock_namespace_without_bypass() {
        let temp = tempfile::TempDir::new().expect("temp lock root");
        let root = temp.path().join("locks");
        let tool = unique_tool_name();
        let first = acquire_named_in(&root, &tool, false).expect("acquire first named lock");
        let second = acquire_named_in(&root, &tool, false)
            .expect_err("second named acquisition should fail");
        assert!(matches!(second, LockError::AlreadyRunning { .. }));
        let first_path = first.path().to_path_buf();
        drop(first);
        fs::remove_file(first_path).ok();
    }
}
