use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use libc::{getegid, geteuid};

use crate::process::{run_status, run_status_streaming, run_status_with_input};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntimeKind {
    Docker,
    Podman,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectionMode {
    UserOnly,
    UserOrSudo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerRuntime {
    kind: ContainerRuntimeKind,
    use_sudo: bool,
}

pub struct ContainerMount<'a> {
    pub source: &'a Path,
    pub target: &'a str,
    pub read_only: bool,
}

impl ContainerRuntime {
    pub fn detect() -> Result<Self> {
        Self::detect_with_mode(DetectionMode::UserOrSudo)
    }

    pub fn detect_with_mode(mode: DetectionMode) -> Result<Self> {
        let mut candidates = vec![
            Self::new(ContainerRuntimeKind::Docker, false),
            Self::new(ContainerRuntimeKind::Podman, false),
        ];
        if mode == DetectionMode::UserOrSudo {
            candidates.push(Self::new(ContainerRuntimeKind::Docker, true));
            candidates.push(Self::new(ContainerRuntimeKind::Podman, true));
        }
        for runtime in candidates {
            if runtime.is_available() {
                return Ok(runtime);
            }
        }

        bail!(
            "Missing supported local container runtime. Install `docker` or `podman`, or make it usable without an interactive sudo prompt."
        )
    }

    pub const fn new(kind: ContainerRuntimeKind, use_sudo: bool) -> Self {
        Self { kind, use_sudo }
    }

    pub fn name(self) -> &'static str {
        match self.kind {
            ContainerRuntimeKind::Docker => "docker",
            ContainerRuntimeKind::Podman => "podman",
        }
    }

    pub const fn uses_sudo(self) -> bool {
        self.use_sudo
    }

    pub fn command(self) -> Command {
        if self.use_sudo {
            let mut command = Command::new("sudo");
            command.arg("-n").arg(self.name());
            command
        } else {
            Command::new(self.name())
        }
    }

    pub fn shell_prefix(self) -> String {
        if self.use_sudo {
            format!("sudo -n {}", self.name())
        } else {
            self.name().to_owned()
        }
    }

    pub fn ensure_build_available(self) -> Result<()> {
        match self.kind {
            ContainerRuntimeKind::Docker => ensure_docker_buildx_available(self),
            ContainerRuntimeKind::Podman => Ok(()),
        }
    }

    pub fn build(self, tag: &str, context_dir: &Path, containerfile: &Path) -> Result<()> {
        self.ensure_build_available()?;
        let mut command = self.command();
        command.args(self.build_args(tag, context_dir, containerfile));
        run_status_streaming(&mut command, self.build_context())
    }

    pub fn image_exists(self, tag: &str) -> Result<bool> {
        let mut command = self.command();
        command.arg("image").arg("inspect").arg(tag);
        Ok(command.output()?.status.success())
    }

    pub fn save(self, tag: &str, archive_path: &Path) -> Result<()> {
        let mut command = self.command();
        command.arg("save").arg("-o").arg(archive_path).arg(tag);
        run_status(&mut command, "save container image archive")
    }

    pub fn load(self, archive_path: &Path) -> Result<()> {
        let mut command = self.command();
        command.arg("load").arg("-i").arg(archive_path);
        run_status_streaming(&mut command, "load container image archive")
    }

    pub fn tag(self, source: &str, target: &str) -> Result<()> {
        let mut command = self.command();
        command.arg("tag").arg(source).arg(target);
        run_status(&mut command, "tag container image")
    }

    pub fn remove_image(self, target: &str) -> Result<()> {
        let mut command = self.command();
        command.arg("image").arg("rm").arg("-f").arg(target);
        run_status(&mut command, "remove container image")
    }

    pub fn push(self, target: &str) -> Result<()> {
        let mut command = self.command();
        command.arg("push").arg(target);
        run_status_streaming(&mut command, "push container image")
    }

    pub fn pull(self, target: &str) -> Result<()> {
        let mut command = self.command();
        command.arg("pull").arg(target);
        run_status_streaming(&mut command, "pull container image")
    }

    pub fn run(
        self,
        image: &str,
        workdir: Option<&str>,
        envs: &[(String, String)],
        mounts: &[ContainerMount<'_>],
        command_args: &[String],
    ) -> Result<()> {
        let mut command = self.command();
        command.arg("run").arg("--rm");
        for mount in mounts {
            let mut spec = format!("{}:{}", mount.source.display(), mount.target);
            if mount.read_only {
                spec.push_str(":ro");
            }
            command.arg("-v").arg(spec);
        }
        if let Some(workdir) = workdir {
            command.arg("-w").arg(workdir);
        }
        for (key, value) in envs {
            command.arg("-e").arg(format!("{key}={value}"));
        }
        command.arg("--user").arg(current_user_spec());
        command.arg(image);
        command.args(command_args);
        run_status_streaming(&mut command, "run container")
    }

    pub fn login_password_stdin(
        self,
        registry: &str,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let mut command = self.command();
        command
            .arg("login")
            .arg("-u")
            .arg(username)
            .arg("--password-stdin")
            .arg(registry);
        run_status_with_input(
            &mut command,
            "log in container runtime",
            password.as_bytes(),
        )
    }

    fn is_available(self) -> bool {
        let mut command = self.command();
        command.arg("version");
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        command
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn build_args(self, tag: &str, context_dir: &Path, containerfile: &Path) -> Vec<String> {
        match self.kind {
            ContainerRuntimeKind::Docker => vec![
                "buildx".to_owned(),
                "build".to_owned(),
                "--load".to_owned(),
                "-t".to_owned(),
                tag.to_owned(),
                "-f".to_owned(),
                containerfile.display().to_string(),
                context_dir.display().to_string(),
            ],
            ContainerRuntimeKind::Podman => vec![
                "build".to_owned(),
                "-t".to_owned(),
                tag.to_owned(),
                "-f".to_owned(),
                containerfile.display().to_string(),
                context_dir.display().to_string(),
            ],
        }
    }

    fn build_context(self) -> &'static str {
        match self.kind {
            ContainerRuntimeKind::Docker => "build container image with Docker Buildx",
            ContainerRuntimeKind::Podman => "build container image with Podman",
        }
    }
}

fn current_user_spec() -> String {
    unsafe { format!("{}:{}", geteuid(), getegid()) }
}

fn ensure_docker_buildx_available(runtime: ContainerRuntime) -> Result<()> {
    let mut command = runtime.command();
    command.args(["buildx", "version"]);
    let output = command
        .output()
        .context("Failed to check whether Docker Buildx is installed")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if detail.is_empty() {
        bail!(
            "Docker Buildx is required. Install the `docker buildx` plugin; legacy builders are not supported."
        );
    }
    bail!(
        "Docker Buildx is required. Install the `docker buildx` plugin; legacy builders are not supported. Docker reported: {detail}"
    )
}
