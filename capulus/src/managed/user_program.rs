use std::os::unix::process::CommandExt;
use std::path::{Component, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::process::{Pid, Signal, kill_process_group};
use semver::Version;

use crate::Cancellation;

use super::validation::is_valid_identifier;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct UserProgramUpdateOptions {
    pub package: String,
    pub cargo_binary: String,
    pub version: String,
    pub registry: Option<String>,
    pub cargo_root: PathBuf,
    pub timeout: Duration,
}

impl Default for UserProgramUpdateOptions {
    fn default() -> Self {
        Self {
            package: String::new(),
            cargo_binary: String::new(),
            version: String::new(),
            registry: None,
            cargo_root: PathBuf::new(),
            timeout: Duration::from_secs(60 * 60),
        }
    }
}

impl UserProgramUpdateOptions {
    pub fn validate(self) -> Result<UserProgramUpdate> {
        if !is_valid_identifier(&self.package) {
            bail!("user-program Cargo package name is invalid");
        }
        if !is_valid_identifier(&self.cargo_binary) {
            bail!("user-program Cargo binary name is invalid");
        }
        let version = Version::parse(&self.version)
            .context("user-program release version is not semantic")?;
        if !version.pre.is_empty() || !version.build.is_empty() {
            bail!("user-program release must be stable and omit build metadata");
        }
        if let Some(registry) = &self.registry
            && !is_valid_identifier(registry)
        {
            bail!("user-program Cargo registry name is invalid");
        }
        if !self.cargo_root.is_absolute()
            || self.cargo_root == std::path::Path::new("/")
            || self.cargo_root.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::CurDir | Component::Prefix(_)
                )
            })
        {
            bail!("user-program Cargo root must be a normalized absolute path");
        }
        if !(Duration::from_secs(60)..=Duration::from_secs(24 * 60 * 60)).contains(&self.timeout) {
            bail!("user-program update timeout must be between one minute and one day");
        }
        Ok(UserProgramUpdate {
            package: self.package,
            cargo_binary: self.cargo_binary,
            version,
            registry: self.registry,
            cargo_root: self.cargo_root,
            timeout: self.timeout,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UserProgramUpdate {
    package: String,
    cargo_binary: String,
    version: Version,
    registry: Option<String>,
    cargo_root: PathBuf,
    timeout: Duration,
}

impl UserProgramUpdate {
    pub fn is_current(&self) -> bool {
        let output = Command::new("/usr/bin/timeout")
            .args(["--signal=TERM", "--kill-after=2s", "10s"])
            .arg(self.cargo_root.join("bin").join(&self.cargo_binary))
            .arg("--version")
            .stdin(Stdio::null())
            .output();
        let Ok(output) = output else {
            return false;
        };
        let expected = self.version.to_string();
        output.status.success()
            && output.stdout.len() <= 4096
            && output.stderr.len() <= 4096
            && String::from_utf8(output.stdout)
                .is_ok_and(|stdout| stdout.split_whitespace().last() == Some(expected.as_str()))
    }

    pub fn install(&self, cancellation: Cancellation) -> Result<()> {
        if rustix::process::geteuid().is_root() {
            bail!("a user-program update must not run as root");
        }
        let mut command = self.command();
        command.process_group(0);
        let mut child = command.spawn().with_context(|| {
            format!("failed to start Cargo while updating {}", self.cargo_binary)
        })?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().with_context(|| {
                format!("failed to wait for the {} Cargo update", self.cargo_binary)
            })? {
                if status.success() {
                    return Ok(());
                }
                bail!(
                    "Cargo failed to install {} v{}: {status}",
                    self.cargo_binary,
                    self.version
                );
            }
            if let Err(cancelled) = cancellation.check() {
                terminate(&mut child)?;
                return Err(cancelled.into());
            }
            if started.elapsed() >= self.timeout {
                terminate(&mut child)?;
                bail!(
                    "timed out after {} seconds updating {}",
                    self.timeout.as_secs(),
                    self.cargo_binary
                );
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.cargo_root.join("bin/cargo"));
        command
            .args(["install", "--locked", "--force", "--version"])
            .arg(self.version.to_string())
            .arg(&self.package)
            .args(["--bin", &self.cargo_binary])
            .arg("--root")
            .arg(&self.cargo_root);
        if let Some(registry) = &self.registry {
            command.args(["--registry", registry]);
        }
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        command
    }
}

pub fn current_user_cargo_root() -> Result<PathBuf> {
    if rustix::process::geteuid().is_root() {
        bail!("a user-program Cargo root cannot be resolved for root");
    }
    let home = dirs::home_dir().context("current user has no home directory")?;
    if !home.is_absolute() {
        bail!("current user home directory is not absolute");
    }
    Ok(home.join(".cargo"))
}

fn terminate(child: &mut std::process::Child) -> Result<()> {
    if let Some(pid) = Pid::from_raw(child.id() as i32) {
        let _ = kill_process_group(pid, Signal::TERM);
        let deadline = Instant::now() + TERMINATION_GRACE;
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        if child.try_wait()?.is_none() {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn update_command_installs_one_exact_binary() {
        let update = UserProgramUpdateOptions {
            package: "aegis-tool".to_string(),
            cargo_binary: "aegis".to_string(),
            version: "1.2.3".to_string(),
            registry: Some("hoek-deus".to_string()),
            cargo_root: PathBuf::from("/home/example/.cargo"),
            ..UserProgramUpdateOptions::default()
        }
        .validate()
        .unwrap();
        let command = update.command();
        assert_eq!(command.get_program(), "/home/example/.cargo/bin/cargo");
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "install",
                "--locked",
                "--force",
                "--version",
                "1.2.3",
                "aegis-tool",
                "--bin",
                "aegis",
                "--root",
                "/home/example/.cargo",
                "--registry",
                "hoek-deus",
            ]
        );
    }

    #[test]
    fn current_version_is_read_from_the_declared_user_installation() {
        let directory = tempfile::tempdir().unwrap();
        let binary_directory = directory.path().join("bin");
        fs::create_dir(&binary_directory).unwrap();
        let binary = binary_directory.join("aegis");
        fs::write(&binary, "#!/bin/sh\nprintf 'aegis 1.2.3\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let update = UserProgramUpdateOptions {
            package: "aegis-tool".to_string(),
            cargo_binary: "aegis".to_string(),
            version: "1.2.3".to_string(),
            cargo_root: directory.path().to_path_buf(),
            ..UserProgramUpdateOptions::default()
        }
        .validate()
        .unwrap();

        assert!(update.is_current());
    }
}
