use std::fs::{self, File};
use std::io::{Read, Seek};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::process::{Pid, Signal, kill_process_group};
use sha2::{Digest, Sha256};

use super::account::{SupplementaryGroups, ensure_owned_directory, require_root};
use super::user_install::remove_orphaned_user_installations;
use super::{BuildAccount, InstallationManifest, ManagedProduct, RedeployRequest, UnixAccount};

const GLOBAL_BUILD_LOCK_ROOT: &str = "/run/capulus/locks";
const GLOBAL_BUILD_LOCK_NAME: &str = "managed-build";
const RUSTUP_DOWNLOAD_LIMIT: u64 = 64 * 1024 * 1024;
const TOOLCHAIN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const RUSTUP_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(200);

pub struct ManagedBuild {
    account: BuildAccount,
    request: RedeployRequest,
    job_root: PathBuf,
    cargo_home: PathBuf,
    install_root: PathBuf,
    _global_lock: crate::InvocationLock,
}

impl ManagedBuild {
    pub fn prepare(request: RedeployRequest) -> Result<Self> {
        require_root()?;
        fs::create_dir_all(GLOBAL_BUILD_LOCK_ROOT)
            .context("failed to create global Capulus build lock directory")?;
        fs::set_permissions(GLOBAL_BUILD_LOCK_ROOT, fs::Permissions::from_mode(0o700))
            .context("failed to secure global Capulus build lock directory")?;
        let global_lock =
            crate::acquire_named_in(GLOBAL_BUILD_LOCK_ROOT, GLOBAL_BUILD_LOCK_NAME, true)
                .context("failed to acquire global Capulus build lock")?;
        let account = BuildAccount::ensure()?;
        account.remove_orphaned_jobs()?;
        remove_orphaned_user_installations()?;
        let job_root = account
            .jobs_home
            .join(format!("{}-{}", request.product, request.job));
        if fs::symlink_metadata(&job_root).is_ok() {
            bail!(
                "Capulus build job path already exists: {}",
                job_root.display()
            );
        }
        ensure_owned_directory(&job_root, &account.account, 0o700)?;
        let cargo_home = job_root.join("cargo");
        let install_root = job_root.join("install");
        ensure_owned_directory(&cargo_home, &account.account, 0o700)?;
        ensure_owned_directory(&install_root, &account.account, 0o700)?;
        for cache in ["registry", "git"] {
            let shared = account.cache_home.join(cache);
            ensure_owned_directory(&shared, &account.account, 0o700)?;
            symlink(&shared, cargo_home.join(cache)).with_context(|| {
                format!("failed to link shared Cargo {cache} cache into build job")
            })?;
        }
        Ok(Self {
            account,
            request,
            job_root,
            cargo_home,
            install_root,
            _global_lock: global_lock,
        })
    }

    pub fn ensure_toolchain(&self) -> Result<()> {
        if !self.account.cargo().is_file() || !self.account.rustup().is_file() {
            self.bootstrap_rustup()?;
        }
        let mut command = self.toolchain_command(self.account.rustup());
        command.args(["toolchain", "install", "stable", "--profile", "minimal"]);
        run_with_deadline(
            &mut command,
            TOOLCHAIN_TIMEOUT,
            "install stable Rust toolchain",
        )?;
        let mut command = self.toolchain_command(self.account.rustup());
        command.args(["default", "stable"]);
        run_with_deadline(
            &mut command,
            Duration::from_secs(60),
            "select stable Rust toolchain",
        )?;
        Ok(())
    }

    pub fn compile(&self, product: &ManagedProduct) -> Result<BuildArtifacts> {
        if self.request.product != product.name() || self.request.package != product.package() {
            bail!("build request does not match the managed product");
        }
        self.write_registry_configuration()?;
        let mut command = self.build_account_command(self.account.cargo());
        command.args([
            "install",
            "--locked",
            "--force",
            "--root",
            self.install_root
                .to_str()
                .ok_or_else(|| anyhow!("build install root is not UTF-8"))?,
            "--version",
            &self.request.release.version.to_string(),
            &self.request.package,
            "--bins",
        ]);
        if let Some(registry) = self.request.release.registry.cargo_registry_name() {
            command.args(["--registry", registry]);
        }
        let result = run_with_deadline(
            &mut command,
            product.build_timeout(),
            "compile managed Cargo release",
        );
        self.remove_registry_secrets()?;
        result?;
        let artifacts = BuildArtifacts {
            binary_directory: self.install_root.join("bin"),
            manifest: self.read_installation_manifest(product)?,
        };
        artifacts.validate(product, &self.account.account)?;
        self.validate_binary_versions(product, &artifacts)?;
        Ok(artifacts)
    }

    pub fn account(&self) -> &BuildAccount {
        &self.account
    }

    fn validate_binary_versions(
        &self,
        product: &ManagedProduct,
        artifacts: &BuildArtifacts,
    ) -> Result<()> {
        let expected = self.request.release.version.to_string();
        for binary in product.system_binaries() {
            let output_path = self
                .job_root
                .join(format!("{}-version.txt", binary.cargo_name));
            let mut output = self.account.account.create_file(&output_path, 0o600)?;
            let mut command =
                self.build_account_command(artifacts.binary_directory.join(&binary.cargo_name));
            command
                .arg("--version")
                .stdout(Stdio::from(output.try_clone()?));
            run_with_deadline(
                &mut command,
                Duration::from_secs(30),
                "query staged binary version",
            )?;
            let value = read_bounded_output(&mut output, 4096, "staged binary version")?;
            fs::remove_file(&output_path)?;
            if value.split_whitespace().last() != Some(expected.as_str()) {
                bail!(
                    "staged binary {} did not report requested version {}",
                    binary.cargo_name,
                    expected
                );
            }
        }
        Ok(())
    }

    fn bootstrap_rustup(&self) -> Result<()> {
        let target = rustup_target()?;
        let base_url = format!("https://static.rust-lang.org/rustup/dist/{target}/rustup-init");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(format!("capulus/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create Rustup bootstrap HTTP client")?;
        let checksum = client
            .get(format!("{base_url}.sha256"))
            .send()
            .context("failed to download Rustup checksum")?
            .error_for_status()
            .context("Rustup checksum request failed")?
            .text()
            .context("failed to read Rustup checksum")?;
        let expected = checksum
            .split_whitespace()
            .next()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow!("Rustup checksum response is malformed"))?
            .to_ascii_lowercase();
        let response = client
            .get(base_url)
            .send()
            .context("failed to download Rustup installer")?
            .error_for_status()
            .context("Rustup installer request failed")?;
        if response
            .content_length()
            .is_some_and(|length| length > RUSTUP_DOWNLOAD_LIMIT)
        {
            bail!("Rustup installer exceeds the download safety limit");
        }
        let bootstrap_directory = self.job_root.join("bootstrap");
        ensure_owned_directory(&bootstrap_directory, &self.account.account, 0o700)?;
        let installer = bootstrap_directory.join("rustup-init");
        let mut bytes = Vec::new();
        response
            .take(RUSTUP_DOWNLOAD_LIMIT + 1)
            .read_to_end(&mut bytes)
            .context("failed to read Rustup installer")?;
        if bytes.len() as u64 > RUSTUP_DOWNLOAD_LIMIT {
            bail!("Rustup installer exceeds the download safety limit");
        }
        if hex_digest(&bytes) != expected {
            bail!("Rustup installer checksum does not match its published SHA-256");
        }
        self.account.account.write_file(&installer, &bytes, 0o500)?;
        let mut command = self.toolchain_command(&installer);
        command.args([
            "-y",
            "--profile",
            "minimal",
            "--no-modify-path",
            "--default-toolchain",
            "none",
        ]);
        run_with_deadline(&mut command, RUSTUP_BOOTSTRAP_TIMEOUT, "bootstrap Rustup")?;
        fs::remove_file(&installer).context("failed to remove Rustup bootstrap installer")
    }

    fn write_registry_configuration(&self) -> Result<()> {
        let ca_path = self.cargo_home.join("registry-ca.pem");
        let configuration = self.request.release.registry.configuration(&ca_path)?;
        if let Some(credentials) = configuration.credentials {
            self.account.account.write_file(
                &self.cargo_home.join("credentials.toml"),
                credentials.as_bytes(),
                0o600,
            )?;
            self.account.account.write_file(
                &ca_path,
                configuration
                    .ca_pem
                    .expect("private Cargo credentials always have a CA bundle")
                    .as_bytes(),
                0o600,
            )?;
        }
        self.account.account.write_file(
            &self.cargo_home.join("config.toml"),
            configuration.config.as_bytes(),
            0o600,
        )
    }

    fn remove_registry_secrets(&self) -> Result<()> {
        for name in ["credentials.toml", "registry-ca.pem"] {
            match fs::remove_file(self.cargo_home.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("failed to remove ephemeral Cargo secret"),
            }
        }
        Ok(())
    }

    fn read_installation_manifest(&self, product: &ManagedProduct) -> Result<InstallationManifest> {
        let agent_name = product
            .agent_executable()
            .file_name()
            .ok_or_else(|| anyhow!("managed agent executable has no filename"))?;
        let staged_agent = self.install_root.join("bin").join(agent_name);
        let output_path = self.job_root.join("installation-manifest.json");
        let mut output = self.account.account.create_file(&output_path, 0o600)?;
        let mut command = self.build_account_command(&staged_agent);
        command.arg("installation-manifest");
        command.stdout(Stdio::from(output.try_clone()?));
        run_with_deadline(
            &mut command,
            Duration::from_secs(30),
            "render staged installation manifest",
        )?;
        let manifest = InstallationManifest::from_json(&read_bounded_output(
            &mut output,
            256 * 1024,
            "staged installation manifest",
        )?)
        .context("staged agent returned an invalid installation manifest")?;
        fs::remove_file(output_path).context("failed to remove staged installation manifest")?;
        product.validate_release_manifest(&manifest, &self.request.release.version)?;
        Ok(manifest)
    }

    fn build_account_command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        self.account_command(program, &self.cargo_home)
    }

    fn toolchain_command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        self.account_command(program, &self.account.cargo_tools_home)
    }

    fn account_command(&self, program: impl AsRef<std::ffi::OsStr>, cargo_home: &Path) -> Command {
        let mut command = self.account.command(program, cargo_home);
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        command
    }
}

impl Drop for ManagedBuild {
    fn drop(&mut self) {
        if self.job_root.parent() == Some(self.account.jobs_home.as_path()) {
            let _ = fs::remove_dir_all(&self.job_root);
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildArtifacts {
    pub binary_directory: PathBuf,
    pub manifest: InstallationManifest,
}

impl BuildArtifacts {
    pub fn from_local_directory(
        product: &ManagedProduct,
        binary_directory: impl Into<PathBuf>,
        account: &UnixAccount,
    ) -> Result<Self> {
        require_root()?;
        account.validate_interactive()?;
        let binary_directory = binary_directory.into();
        validate_local_artifact_directory(&binary_directory, account)?;
        let artifacts = Self {
            binary_directory,
            manifest: product.installation_manifest(),
        };
        product.validate_release_manifest(&artifacts.manifest, product.version())?;
        artifacts.validate(product, account)?;
        artifacts.validate_versions(product, account)?;
        Ok(artifacts)
    }

    pub(crate) fn validate(&self, product: &ManagedProduct, account: &UnixAccount) -> Result<()> {
        product.validate_release_manifest(&self.manifest, &self.manifest.version)?;
        for file in &self.manifest.files {
            let super::ManagedFile::Binary { source_name, .. } = file else {
                continue;
            };
            let path = self.binary_directory.join(source_name);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("missing staged binary {}", path.display()))?;
            if !metadata.file_type().is_file()
                || metadata.uid() != account.uid
                || metadata.gid() != account.gid
                || metadata.permissions().mode() & 0o111 == 0
            {
                bail!(
                    "staged artifact is not an executable regular file owned by the source account: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }

    fn validate_versions(&self, product: &ManagedProduct, account: &UnixAccount) -> Result<()> {
        let expected_version = product.version().to_string();
        for binary in product.system_binaries() {
            let mut output =
                tempfile::tempfile().context("failed to create version output file")?;
            let mut command = account.command(
                self.binary_directory.join(&binary.cargo_name),
                SupplementaryGroups::Initialize,
            );
            command
                .arg("--version")
                .stdout(Stdio::from(output.try_clone()?))
                .stderr(Stdio::null());
            run_with_deadline(
                &mut command,
                Duration::from_secs(30),
                "query local installation artifact version",
            )?;
            output.rewind()?;
            let mut value = String::new();
            output.take(4097).read_to_string(&mut value)?;
            if value.len() > 4096 || value.split_whitespace().last() != Some(&expected_version) {
                bail!(
                    "local artifact {} did not report expected version {}",
                    binary.cargo_name,
                    product.version()
                );
            }
        }
        Ok(())
    }
}

fn validate_local_artifact_directory(path: &Path, account: &UnixAccount) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::CurDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("local artifact directory must be normalized and absolute");
    }
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "failed to inspect local artifact directory {}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != account.uid
        || metadata.gid() != account.gid
    {
        bail!(
            "local artifact directory is not a real directory owned by {}",
            account.name
        );
    }
    Ok(())
}

fn rustup_target() -> Result<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        (architecture, operating_system) => bail!(
            "Capulus has no audited Rustup bootstrap target for {architecture}-{operating_system}"
        ),
    }
}

fn read_bounded_output(file: &mut File, limit: u64, description: &str) -> Result<String> {
    if file.metadata()?.len() > limit {
        bail!("{description} exceeds the {limit}-byte safety limit");
    }
    file.rewind()?;
    let mut value = String::new();
    file.take(limit + 1).read_to_string(&mut value)?;
    if value.len() as u64 > limit {
        bail!("{description} exceeds the {limit}-byte safety limit");
    }
    Ok(value)
}

pub(super) fn run_with_deadline(
    command: &mut Command,
    timeout: Duration,
    action: &str,
) -> Result<ExitStatus> {
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to {action}"))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait while trying to {action}"))?
        {
            if status.success() {
                return Ok(status);
            }
            bail!("failed to {action}: child exited with status {status}");
        }
        if started.elapsed() >= timeout {
            if let Some(pid) = Pid::from_raw(child.id() as i32) {
                let _ = kill_process_group(pid, Signal::TERM);
                let termination_started = Instant::now();
                while child.try_wait()?.is_none() {
                    if termination_started.elapsed() >= Duration::from_secs(2) {
                        let _ = kill_process_group(pid, Signal::KILL);
                        break;
                    }
                    thread::sleep(COMMAND_POLL_INTERVAL);
                }
            }
            let _ = child.wait();
            bail!(
                "timed out after {} seconds trying to {action}",
                timeout.as_secs()
            );
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::CargoRegistry;

    #[test]
    fn rustup_target_is_explicit_for_supported_linux_architectures() {
        if matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            assert!(rustup_target().unwrap().ends_with("-unknown-linux-gnu"));
        }
    }

    #[test]
    fn private_registry_debug_redacts_credentials() {
        let registry = CargoRegistry::private(
            "private",
            "sparse+https://registry.example/",
            "very-secret-token",
            "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let debug = format!("{registry:?}");
        assert!(!debug.contains("very-secret-token"));
        assert!(!debug.contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn deadline_terminates_the_complete_child_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("descendant.pid");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"; wait", "sh"])
            .arg(&pid_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        assert!(
            run_with_deadline(&mut command, Duration::from_millis(100), "run test child").is_err()
        );
        let pid = fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal zero only queries whether the numeric process ID still exists.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            if !exists {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                break;
            }
            if Instant::now() >= deadline {
                // SAFETY: the test created this still-live descendant and must not leak it.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("timed-out command left descendant PID {pid} alive");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}
