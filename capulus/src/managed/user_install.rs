use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{
    AtFlags, Mode, RawDir, StatVfsMountFlags, fstatvfs, mkdirat, symlinkat, unlinkat,
};

use super::account::{
    DirectoryIdentity, DirectoryOwner, SupplementaryGroups, UnixAccount, ensure_root_directory,
    open_directory_at, open_file_at, open_real_directory, require_empty_directory, require_root,
    set_directory_mode_and_sync, set_directory_owner_and_sync,
};
use super::build::{acquire_managed_build_lock, run_with_deadline, try_acquire_managed_build_lock};
use super::product::is_valid_identifier;
use super::{
    BuildAccount, CargoRegistry, JobId, ManagedProduct, ResolvedRelease, UserInstallContext,
};

const USER_INSTALL_STATE_ROOT: &str = "/var/lib/capulus-user-installs";
const USER_INSTALL_RUNTIME_ROOT: &str = "/run/capulus/user-installs";
const CARGO_CONFIG: &str = "config.toml";
const CARGO_CREDENTIALS: &str = "credentials.toml";
const REGISTRY_CA: &str = "registry-ca.pem";
const ORPHAN_CLEANUP_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const ORPHAN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);

pub fn reinstall_user_cli(
    product: &ManagedProduct,
    job: JobId,
    release: &ResolvedRelease,
    context: &UserInstallContext,
) -> Result<()> {
    let _lock = acquire_managed_build_lock()?;
    remove_orphaned_user_installations_until_available()?;
    BuildAccount::ensure()?.reclaim_target_home()?;
    let mut workspace = PreparedUserInstall::prepare_process_managed(product, job, context)?;
    let attempt = workspace.reinstall_and_verify(product, release, context);
    combine_results(attempt.result, workspace.cleanup())
}

pub(super) struct PreparedUserInstall {
    account: UnixAccount,
    name: String,
    state_root: PathBuf,
    runtime_root: PathBuf,
    state_job: PathBuf,
    runtime_job: PathBuf,
    cargo_home: PathBuf,
    target: PathBuf,
    boundary_owner: DirectoryOwner,
    state_present: bool,
    runtime_present: bool,
    runtime_management: RuntimeManagement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeManagement {
    Process,
    Systemd,
}

pub(super) struct UserReinstallAttempt {
    pub(super) outcome: bool,
    pub(super) result: Result<()>,
}

impl UserReinstallAttempt {
    fn verified() -> Self {
        Self {
            outcome: true,
            result: Ok(()),
        }
    }

    fn failed(error: anyhow::Error) -> Self {
        Self {
            outcome: false,
            result: Err(error),
        }
    }
}

impl PreparedUserInstall {
    pub(super) fn prepare(
        product: &ManagedProduct,
        job: JobId,
        context: &UserInstallContext,
    ) -> Result<Self> {
        Self::prepare_managed(product, job, context, RuntimeManagement::Systemd)
    }

    fn prepare_process_managed(
        product: &ManagedProduct,
        job: JobId,
        context: &UserInstallContext,
    ) -> Result<Self> {
        Self::prepare_managed(product, job, context, RuntimeManagement::Process)
    }

    fn prepare_managed(
        product: &ManagedProduct,
        job: JobId,
        context: &UserInstallContext,
        runtime_management: RuntimeManagement,
    ) -> Result<Self> {
        require_root()?;
        ensure_root_directory(Path::new("/run/capulus"), 0o711)?;
        let account = context.revalidate(&product.user_binary().cargo_name)?;
        Self::prepare_at(
            Path::new(USER_INSTALL_STATE_ROOT),
            Path::new(USER_INSTALL_RUNTIME_ROOT),
            product.name(),
            job,
            account,
            DirectoryOwner::ROOT,
            runtime_management,
        )
    }

    fn prepare_at(
        state_root: &Path,
        runtime_root: &Path,
        product: &str,
        job: JobId,
        account: UnixAccount,
        boundary_owner: DirectoryOwner,
        runtime_management: RuntimeManagement,
    ) -> Result<Self> {
        let state_directory = WorkspaceRoot::ensure(state_root, boundary_owner)?;
        require_executable_filesystem(state_root, fstatvfs(&state_directory.directory)?.f_flag)?;
        let runtime_directory = WorkspaceRoot::ensure(runtime_root, boundary_owner)?;
        let name = format!("{product}-{job}");
        validate_workspace_name(&name)?;
        let mut workspace = Self {
            account,
            name: name.clone(),
            state_root: state_root.to_path_buf(),
            runtime_root: runtime_root.to_path_buf(),
            state_job: state_root.join(&name),
            runtime_job: runtime_root.join(&name),
            cargo_home: state_root.join(&name).join("cargo"),
            target: state_root.join(&name).join("target"),
            boundary_owner,
            state_present: false,
            runtime_present: false,
            runtime_management,
        };
        let result = (|| {
            state_directory.create_job(&name, ExistingJob::Reject)?;
            workspace.state_present = true;
            state_directory.secure_job(&name)?;
            let state_job = state_directory.open_job(&name)?;
            create_user_directory(&state_job, "cargo", &workspace.account)?;
            create_user_directory(&state_job, "target", &workspace.account)?;
            runtime_directory.create_job(&name, ExistingJob::AcceptEmpty)?;
            workspace.runtime_present = true;
            runtime_directory.secure_job(&name)?;
            Ok(())
        })();
        if let Err(error) = result {
            return Err(combine_error(error, workspace.cleanup()));
        }
        Ok(workspace)
    }

    pub(super) fn reinstall_and_verify(
        &mut self,
        product: &ManagedProduct,
        release: &ResolvedRelease,
        context: &UserInstallContext,
    ) -> UserReinstallAttempt {
        let prepared = (|| {
            let account = context.revalidate(&product.user_binary().cargo_name)?;
            self.validate_account(&account)?;
            let state = WorkspaceRoot::ensure(&self.state_root, self.boundary_owner)?;
            open_owned_child_directory(&state.open_job(&self.name)?, "target", &account)?;
            self.populate_runtime_configuration(&release.registry, &account)?;
            Ok(account)
        })();
        let account = match prepared {
            Ok(account) => account,
            Err(error) => return UserReinstallAttempt::failed(error),
        };
        let mut command = user_install_command(
            &account,
            context,
            &self.cargo_home,
            &self.target,
            release,
            product.package(),
            &product.user_binary().cargo_name,
        );
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let cargo = run_with_deadline(
            &mut command,
            product.build_timeout(),
            "reinstall the requesting user's Cargo CLI",
        )
        .map(|_| ());
        let scrub = self.remove_runtime_workspace();
        if let Err(error) = cargo {
            return UserReinstallAttempt::failed(combine_error(error, scrub));
        }
        let verification = (|| {
            let account = context.revalidate(&product.user_binary().cargo_name)?;
            self.validate_account(&account)?;
            verify_installed_version(release, context, &self.state_job, &account)
        })();
        finish_verified_reinstall(verification, scrub)
    }

    fn validate_account(&self, account: &UnixAccount) -> Result<()> {
        if account.uid != self.account.uid || account.gid != self.account.gid {
            bail!("requesting user's account changed after preparing its Cargo workspace");
        }
        Ok(())
    }

    fn populate_runtime_configuration(
        &self,
        registry: &CargoRegistry,
        account: &UnixAccount,
    ) -> Result<()> {
        let state = WorkspaceRoot::ensure(&self.state_root, self.boundary_owner)?;
        let cargo_home =
            open_owned_child_directory(&state.open_job(&self.name)?, "cargo", account)?;
        let runtime =
            WorkspaceRoot::ensure(&self.runtime_root, self.boundary_owner)?.open_job(&self.name)?;
        let ca_path = self.runtime_job.join(REGISTRY_CA);
        let configuration = registry.configuration(&ca_path)?;
        account.write_file(
            &self.runtime_job.join(CARGO_CONFIG),
            configuration.config.as_bytes(),
            0o600,
        )?;
        link_runtime_file(&cargo_home, &self.runtime_job, CARGO_CONFIG)?;
        if let Some(credentials) = configuration.credentials {
            account.write_file(
                &self.runtime_job.join(CARGO_CREDENTIALS),
                credentials.as_bytes(),
                0o600,
            )?;
            account.write_file(
                &self.runtime_job.join(REGISTRY_CA),
                configuration
                    .ca_pem
                    .expect("private Cargo credentials always have a CA bundle")
                    .as_bytes(),
                0o600,
            )?;
            link_runtime_file(&cargo_home, &self.runtime_job, CARGO_CREDENTIALS)?;
        }
        runtime.sync_all()?;
        cargo_home.sync_all().map_err(Into::into)
    }

    fn remove_runtime_workspace(&mut self) -> Result<()> {
        if !self.runtime_present {
            return Ok(());
        }
        match self.runtime_management {
            RuntimeManagement::Process => remove_known_workspace(
                &self.runtime_root,
                &self.runtime_job,
                self.boundary_owner,
                WorkspaceKind::Runtime,
            )?,
            RuntimeManagement::Systemd => scrub_systemd_runtime_workspace(
                &self.runtime_root,
                &self.runtime_job,
                self.boundary_owner,
            )?,
        }
        self.runtime_present = false;
        Ok(())
    }

    pub(super) fn cleanup(&mut self) -> Result<()> {
        self.remove_runtime_workspace()?;
        if self.state_present {
            remove_known_workspace(
                &self.state_root,
                &self.state_job,
                self.boundary_owner,
                WorkspaceKind::State,
            )
            .map(|()| self.state_present = false)?;
        }
        Ok(())
    }
}

impl Drop for PreparedUserInstall {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn user_install_command(
    account: &UnixAccount,
    context: &UserInstallContext,
    cargo_home: &Path,
    target: &Path,
    release: &ResolvedRelease,
    package: &str,
    binary: &str,
) -> Command {
    let mut command = account.command(&context.cargo, SupplementaryGroups::Initialize);
    command
        .env("CARGO_HOME", cargo_home)
        .env("RUSTUP_HOME", &context.rustup_home)
        .env("CARGO_TARGET_DIR", target)
        .env("PATH", user_path(&context.cargo_home))
        .args(["install", "--locked", "--force", "--root"])
        .arg(&context.cargo_home)
        .args(["--version", &release.version.to_string()])
        .arg(package)
        .args(["--bin", binary]);
    if let Some(registry) = release.registry.cargo_registry_name() {
        command.args(["--registry", registry]);
    }
    command
}

fn verify_installed_version(
    release: &ResolvedRelease,
    context: &UserInstallContext,
    workspace: &Path,
    account: &UnixAccount,
) -> Result<()> {
    let output = workspace.join("installed-version.txt");
    let mut output_file = account.create_file(&output, 0o600)?;
    let mut command = account.command(&context.installed_binary, SupplementaryGroups::Initialize);
    command
        .arg("--version")
        .stdout(Stdio::from(output_file.try_clone()?))
        .stderr(Stdio::inherit());
    run_with_deadline(
        &mut command,
        std::time::Duration::from_secs(30),
        "verify the reinstalled user CLI version",
    )?;
    if read_bounded_output(&mut output_file, 4096)?
        .split_whitespace()
        .last()
        != Some(release.version.to_string().as_str())
    {
        bail!(
            "requesting user's reinstalled CLI did not report version {}",
            release.version
        );
    }
    Ok(())
}

struct WorkspaceRoot {
    path: PathBuf,
    directory: File,
    owner: DirectoryOwner,
}

impl WorkspaceRoot {
    fn ensure(path: &Path, owner: DirectoryOwner) -> Result<Self> {
        let parent_path = path
            .parent()
            .ok_or_else(|| anyhow!("Capulus user-install root has no parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow!("Capulus user-install root has no name"))?;
        let parent = open_real_directory(parent_path)?;
        let created = match mkdirat(&parent, name, Mode::from_raw_mode(0o711)) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
        let directory = open_directory_at(&parent, name)?;
        let identity = DirectoryIdentity::from_metadata(&directory.metadata()?);
        if created || identity.is_restricted(owner, 0o711) {
            require_empty_directory(&directory, path)?;
            set_directory_mode_and_sync(
                &directory,
                0o711,
                "failed to normalize a Capulus user-install root",
            )?;
            set_directory_owner_and_sync(
                &directory,
                owner,
                "failed to secure a Capulus user-install root",
            )?;
        } else if !identity.is_owned_directory(owner, 0o711) {
            bail!(
                "Capulus user-install root has invalid type, ownership, or mode: {}",
                path.display()
            );
        }
        parent.sync_all()?;
        Ok(Self {
            path: path.to_path_buf(),
            directory,
            owner,
        })
    }

    fn create_job(&self, name: &str, existing: ExistingJob) -> Result<()> {
        validate_workspace_name(name)?;
        match mkdirat(&self.directory, name, Mode::from_raw_mode(0o711)) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) if existing == ExistingJob::AcceptEmpty => {
                let directory = self.open_job(name)?;
                require_empty_directory(&directory, &self.path.join(name))
            }
            Err(error) => Err(std::io::Error::from(error))
                .with_context(|| format!("failed to create Capulus user-install workspace {name}")),
        }
    }

    fn secure_job(&self, name: &str) -> Result<()> {
        let directory = open_directory_at(&self.directory, OsStr::new(name))?;
        set_directory_mode_and_sync(
            &directory,
            0o711,
            "failed to normalize a Capulus user-install workspace",
        )?;
        set_directory_owner_and_sync(
            &directory,
            self.owner,
            "failed to secure a Capulus user-install workspace",
        )?;
        self.directory.sync_all().map_err(Into::into)
    }

    fn open_job(&self, name: &str) -> Result<File> {
        let directory = open_directory_at(&self.directory, OsStr::new(name))?;
        if !DirectoryIdentity::from_metadata(&directory.metadata()?)
            .is_owned_directory(self.owner, 0o711)
        {
            bail!("Capulus user-install workspace ownership or mode changed: {name:?}");
        }
        Ok(directory)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ExistingJob {
    Reject,
    AcceptEmpty,
}

fn create_user_directory(parent: &File, name: &str, account: &UnixAccount) -> Result<()> {
    mkdirat(parent, name, Mode::from_raw_mode(0o700))
        .map_err(std::io::Error::from)
        .with_context(|| format!("failed to create user Cargo directory {name}"))?;
    let directory = open_directory_at(parent, OsStr::new(name))?;
    set_directory_mode_and_sync(
        &directory,
        0o700,
        "failed to normalize a requesting-user Cargo directory",
    )?;
    set_directory_owner_and_sync(
        &directory,
        DirectoryOwner::of(account),
        "failed to secure a requesting-user Cargo directory",
    )?;
    parent.sync_all().map_err(Into::into)
}

fn open_owned_child_directory(parent: &File, name: &str, account: &UnixAccount) -> Result<File> {
    let directory = open_directory_at(parent, OsStr::new(name))?;
    if !DirectoryIdentity::from_metadata(&directory.metadata()?)
        .is_owned_directory(DirectoryOwner::of(account), 0o700)
    {
        bail!("requesting user's Cargo {name} directory ownership or mode changed");
    }
    Ok(directory)
}

fn link_runtime_file(cargo_home: &File, runtime: &Path, name: &str) -> Result<()> {
    symlinkat(runtime.join(name), cargo_home, name)
        .map_err(std::io::Error::from)
        .with_context(|| format!("failed to link volatile Cargo {name}"))
}

fn require_executable_filesystem(path: &Path, flags: StatVfsMountFlags) -> Result<()> {
    if flags.contains(StatVfsMountFlags::NOEXEC) {
        bail!(
            "Capulus user-install workspace is on a noexec filesystem: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_workspace_name(name: &str) -> Result<()> {
    let Some((product, job)) = name.rsplit_once('-') else {
        bail!("Capulus user-install workspace has an invalid name: {name:?}");
    };
    if !is_valid_identifier(product) || JobId::parse(job).is_err() {
        bail!("Capulus user-install workspace has an invalid name: {name:?}");
    }
    Ok(())
}

fn remove_known_workspace(
    root: &Path,
    workspace: &Path,
    boundary_owner: DirectoryOwner,
    kind: WorkspaceKind,
) -> Result<()> {
    if workspace.parent() != Some(root) {
        bail!("Capulus user-install cleanup path escaped its root");
    }
    let name = workspace
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("Capulus user-install workspace name is not UTF-8"))?;
    validate_workspace_name(name)?;
    let root = WorkspaceRoot::ensure(root, boundary_owner)?;
    validate_workspace_directory(&root.directory, name, boundary_owner, kind)?;
    fs::remove_dir_all(workspace)
        .with_context(|| format!("failed to remove user-install workspace {name}"))?;
    root.directory.sync_all().map_err(Into::into)
}

fn scrub_systemd_runtime_workspace(
    root: &Path,
    workspace: &Path,
    boundary_owner: DirectoryOwner,
) -> Result<()> {
    if workspace.parent() != Some(root) {
        bail!("Capulus user-install cleanup path escaped its root");
    }
    let name = workspace
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("Capulus user-install workspace name is not UTF-8"))?;
    validate_workspace_name(name)?;
    let root = WorkspaceRoot::ensure(root, boundary_owner)?;
    let directory = root.open_job(name)?;
    scrub_runtime_workspace(&directory)
}

fn scrub_runtime_workspace(directory: &File) -> Result<()> {
    scrub_runtime_entries(
        directory,
        validate_workspace_entries(directory, WorkspaceKind::Runtime)?,
    )
}

fn scrub_runtime_entries(directory: &File, entries: Vec<String>) -> Result<()> {
    for entry in entries {
        match unlinkat(directory, &entry, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => {
                return Err(std::io::Error::from(error)).with_context(|| {
                    format!("failed to scrub volatile user-install file {entry:?}")
                });
            }
        }
    }
    directory.sync_all().map_err(Into::into)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WorkspaceKind {
    State,
    Runtime,
}

fn validate_workspace_directory(
    root: &File,
    name: &str,
    boundary_owner: DirectoryOwner,
    kind: WorkspaceKind,
) -> Result<bool> {
    let directory = open_directory_at(root, OsStr::new(name))
        .with_context(|| format!("failed to open user-install workspace {name}"))?;
    validate_open_workspace_directory(&directory, name, boundary_owner, kind)
        .map(|validation| validation.legacy_runtime)
}

struct WorkspaceValidation {
    legacy_runtime: bool,
    entries: Vec<String>,
}

fn validate_open_workspace_directory(
    directory: &File,
    name: &str,
    boundary_owner: DirectoryOwner,
    kind: WorkspaceKind,
) -> Result<WorkspaceValidation> {
    let identity = DirectoryIdentity::from_metadata(&directory.metadata()?);
    if kind == WorkspaceKind::Runtime
        && identity.is_directory_owned_by_other_uid(boundary_owner, 0o700)
    {
        return Ok(WorkspaceValidation {
            legacy_runtime: true,
            entries: Vec::new(),
        });
    }
    if identity.is_restricted(boundary_owner, 0o711) {
        require_empty_directory(directory, Path::new(name))?;
        return Ok(WorkspaceValidation {
            legacy_runtime: false,
            entries: Vec::new(),
        });
    }
    if !identity.is_owned_directory(boundary_owner, 0o711) {
        bail!("user-install workspace boundary has invalid ownership or mode: {name:?}");
    }
    Ok(WorkspaceValidation {
        legacy_runtime: false,
        entries: validate_workspace_entries(directory, kind)?,
    })
}

struct ValidatedRuntimeWorkspace {
    name: String,
    directory: File,
    legacy: bool,
    entries: Vec<String>,
}

fn validate_runtime_workspace(
    root: &File,
    name: String,
    boundary_owner: DirectoryOwner,
) -> Result<Option<ValidatedRuntimeWorkspace>> {
    let directory = match open_directory_at(root, OsStr::new(&name)) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to open user-install workspace {name}"));
        }
    };
    let validation = validate_open_workspace_directory(
        &directory,
        &name,
        boundary_owner,
        WorkspaceKind::Runtime,
    )?;
    Ok(Some(ValidatedRuntimeWorkspace {
        name,
        directory,
        legacy: validation.legacy_runtime,
        entries: validation.entries,
    }))
}

fn validate_workspace_entries(directory: &File, kind: WorkspaceKind) -> Result<Vec<String>> {
    let entries = directory_names(directory)?;
    let mut validated = Vec::with_capacity(entries.len());
    for entry in entries {
        match kind {
            WorkspaceKind::State if matches!(entry.as_str(), "cargo" | "target") => {
                open_directory_at(directory, OsStr::new(&entry)).with_context(|| {
                    format!("user-install state entry is not a real directory: {entry:?}")
                })?;
                validated.push(entry);
            }
            WorkspaceKind::State if entry == "installed-version.txt" => {
                require_regular_file(directory, &entry)?;
                validated.push(entry);
            }
            WorkspaceKind::Runtime
                if matches!(
                    entry.as_str(),
                    CARGO_CONFIG | CARGO_CREDENTIALS | REGISTRY_CA
                ) =>
            {
                if require_regular_file_if_present(directory, &entry)? {
                    validated.push(entry);
                }
            }
            _ => {
                bail!("user-install workspace contains an unexpected entry: {entry:?}");
            }
        }
    }
    Ok(validated)
}

fn require_regular_file(parent: &File, name: &str) -> Result<()> {
    let file = open_file_at(parent, OsStr::new(name))?;
    if !file.metadata()?.file_type().is_file() {
        bail!("user-install workspace entry is not a regular file: {name:?}");
    }
    Ok(())
}

fn require_regular_file_if_present(parent: &File, name: &str) -> Result<bool> {
    match open_file_at(parent, OsStr::new(name)) {
        Ok(file) if file.metadata()?.file_type().is_file() => Ok(true),
        Ok(_) => bail!("user-install workspace entry is not a regular file: {name:?}"),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn directory_names(directory: &File) -> Result<Vec<String>> {
    let mut buffer = [MaybeUninit::uninit(); 4096];
    let mut entries = RawDir::new(directory, &mut buffer);
    let mut names = Vec::new();
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(std::io::Error::from)?;
        if matches!(entry.file_name().to_bytes(), b"." | b"..") {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| anyhow!("user-install workspace has a non-UTF-8 name"))?
            .to_string();
        names.push(name);
    }
    Ok(names)
}

fn workspace_names(directory: &File) -> Result<Vec<String>> {
    let names = directory_names(directory)?;
    for name in &names {
        validate_workspace_name(name)?;
    }
    Ok(names)
}

fn user_path(cargo_home: &Path) -> OsString {
    let mut value = cargo_home.join("bin").into_os_string();
    value.push(":/usr/local/bin:/usr/bin:/bin");
    value
}

fn read_bounded_output(file: &mut File, limit: u64) -> Result<String> {
    if file.metadata()?.len() > limit {
        bail!("installed version output exceeds the {limit}-byte safety limit");
    }
    file.rewind()?;
    let mut value = String::new();
    file.take(limit + 1).read_to_string(&mut value)?;
    if value.len() as u64 > limit {
        bail!("installed version output exceeds the {limit}-byte safety limit");
    }
    Ok(value)
}

pub(super) fn remove_orphaned_user_installations() -> Result<CleanupDisposition> {
    remove_orphaned_user_installations_at(
        Path::new(USER_INSTALL_STATE_ROOT),
        Path::new(USER_INSTALL_RUNTIME_ROOT),
        DirectoryOwner::ROOT,
        None,
    )
}

pub(super) fn remove_orphaned_user_installations_for_job(product: &str, job: JobId) -> Result<()> {
    let name = format!("{product}-{job}");
    validate_workspace_name(&name)?;
    remove_orphaned_user_installations_at(
        Path::new(USER_INSTALL_STATE_ROOT),
        Path::new(USER_INSTALL_RUNTIME_ROOT),
        DirectoryOwner::ROOT,
        Some(&name),
    )
    .map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CleanupDisposition {
    Complete,
    Busy,
}

pub(super) fn cleanup_inactive_user_installations() -> Result<CleanupDisposition> {
    let Some(_lock) = try_acquire_managed_build_lock()? else {
        return Ok(CleanupDisposition::Busy);
    };
    remove_orphaned_user_installations()
}

fn remove_orphaned_user_installations_until_available() -> Result<()> {
    let started = Instant::now();
    loop {
        match remove_orphaned_user_installations()? {
            CleanupDisposition::Complete => return Ok(()),
            CleanupDisposition::Busy if started.elapsed() < ORPHAN_CLEANUP_TIMEOUT => {
                thread::sleep(ORPHAN_CLEANUP_RETRY_INTERVAL);
            }
            CleanupDisposition::Busy => {
                bail!(
                    "timed out waiting for systemd to release a prior Capulus user-install workspace"
                );
            }
        }
    }
}

fn remove_orphaned_user_installations_at(
    state_root: &Path,
    runtime_root: &Path,
    boundary_owner: DirectoryOwner,
    preserved_runtime_job: Option<&str>,
) -> Result<CleanupDisposition> {
    remove_orphaned_user_installations_at_with(
        state_root,
        runtime_root,
        boundary_owner,
        preserved_runtime_job,
        |path| fs::remove_dir_all(path),
    )
}

fn remove_orphaned_user_installations_at_with(
    state_root: &Path,
    runtime_root: &Path,
    boundary_owner: DirectoryOwner,
    preserved_runtime_job: Option<&str>,
    mut remove_runtime: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<CleanupDisposition> {
    let state = WorkspaceRoot::ensure(state_root, boundary_owner)?;
    let runtime = WorkspaceRoot::ensure(runtime_root, boundary_owner)?;
    let state_names = workspace_names(&state.directory)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let runtime_names = workspace_names(&runtime.directory)?;
    for name in &state_names {
        validate_workspace_directory(&state.directory, name, boundary_owner, WorkspaceKind::State)?;
    }
    let mut runtime_workspaces = Vec::new();
    for name in runtime_names {
        if let Some(workspace) =
            validate_runtime_workspace(&runtime.directory, name, boundary_owner)?
        {
            runtime_workspaces.push(workspace);
        }
    }
    let mut deferred_state_names = BTreeSet::new();
    for workspace in runtime_workspaces {
        if preserved_runtime_job == Some(workspace.name.as_str())
            || (!state_names.contains(&workspace.name) && !workspace.legacy)
        {
            continue;
        }
        if !workspace.legacy {
            scrub_runtime_entries(&workspace.directory, workspace.entries)?;
        }
        match remove_runtime(&runtime.path.join(&workspace.name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::ResourceBusy => {
                deferred_state_names.insert(workspace.name);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to remove orphaned user installation {}",
                        workspace.name
                    )
                });
            }
        }
    }
    runtime.directory.sync_all()?;
    for name in state_names {
        if deferred_state_names.contains(&name) {
            continue;
        }
        fs::remove_dir_all(state.path.join(&name))
            .with_context(|| format!("failed to remove orphaned user installation {name}"))?;
    }
    state.directory.sync_all()?;
    Ok(if deferred_state_names.is_empty() {
        CleanupDisposition::Complete
    } else {
        CleanupDisposition::Busy
    })
}

fn combine_results(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(anyhow!("{first:#}; cleanup also failed: {second:#}")),
    }
}

fn finish_verified_reinstall(
    verification: Result<()>,
    runtime_cleanup: Result<()>,
) -> UserReinstallAttempt {
    match verification {
        Ok(()) if runtime_cleanup.is_ok() => UserReinstallAttempt::verified(),
        Ok(()) => UserReinstallAttempt {
            outcome: true,
            result: runtime_cleanup,
        },
        Err(error) => UserReinstallAttempt::failed(combine_error(error, runtime_cleanup)),
    }
}

fn combine_error(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => anyhow!("{error:#}; cleanup also failed: {cleanup:#}"),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use super::*;

    #[test]
    fn user_path_prefixes_the_callers_cargo_bin() {
        assert_eq!(
            user_path(Path::new("/home/example/.cargo")),
            OsString::from("/home/example/.cargo/bin:/usr/local/bin:/usr/bin:/bin")
        );
    }

    #[test]
    fn noexec_workspace_is_rejected_explicitly() {
        assert!(
            require_executable_filesystem(
                Path::new("/var/lib/capulus-user-installs"),
                StatVfsMountFlags::NOEXEC,
            )
            .is_err()
        );
        assert!(
            require_executable_filesystem(
                Path::new("/var/lib/capulus-user-installs"),
                StatVfsMountFlags::NOSUID | StatVfsMountFlags::NODEV,
            )
            .is_ok()
        );
    }

    #[test]
    fn verified_reinstall_outcome_survives_later_workspace_cleanup_failure() {
        let attempt =
            finish_verified_reinstall(Ok(()), Err(anyhow!("injected runtime cleanup failure")));
        assert!(attempt.outcome);
        assert!(attempt.result.is_err());

        let attempt = finish_verified_reinstall(
            Err(anyhow!("injected version verification failure")),
            Ok(()),
        );
        assert!(!attempt.outcome);
        assert!(attempt.result.is_err());

        assert!(!UserReinstallAttempt::failed(anyhow!("injected preparation failure")).outcome);
    }

    #[test]
    fn command_uses_persistent_build_paths_and_the_users_install_root() {
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let context = test_context(&account);
        let release = ResolvedRelease {
            version: semver::Version::new(1, 2, 3),
            registry: CargoRegistry::CratesIo,
        };
        let command = user_install_command(
            &account,
            &context,
            Path::new("/var/lib/capulus-user-installs/a-0123456789abcdef0123456789abcdef/cargo"),
            Path::new("/var/lib/capulus-user-installs/a-0123456789abcdef0123456789abcdef/target"),
            &release,
            "a-tool",
            "a",
        );
        let environment = command
            .get_envs()
            .filter_map(|(name, value)| Some((name, value?)))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment[OsStr::new("CARGO_HOME")],
            OsStr::new("/var/lib/capulus-user-installs/a-0123456789abcdef0123456789abcdef/cargo")
        );
        assert_eq!(
            environment[OsStr::new("CARGO_TARGET_DIR")],
            OsStr::new("/var/lib/capulus-user-installs/a-0123456789abcdef0123456789abcdef/target")
        );
        assert!(
            environment
                .values()
                .all(|value| !Path::new(value).starts_with("/run"))
        );
        let arguments = command.get_args().collect::<Vec<_>>();
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == [OsStr::new("--root"), context.cargo_home.as_os_str()] })
        );
    }

    #[test]
    fn runtime_registry_material_is_linked_not_copied_into_persistent_storage() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account.clone());
        let registry = CargoRegistry::private(
            "private",
            "sparse+https://registry.example.invalid/index/",
            "secret-token",
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----",
        )
        .unwrap();

        workspace
            .populate_runtime_configuration(&registry, &account)
            .unwrap();

        for name in [CARGO_CONFIG, CARGO_CREDENTIALS] {
            let link = workspace.cargo_home.join(name);
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read_link(link).unwrap(),
                workspace.runtime_job.join(name)
            );
        }
        let config = fs::read_to_string(workspace.runtime_job.join(CARGO_CONFIG)).unwrap();
        assert!(config.contains(workspace.runtime_job.join(REGISTRY_CA).to_str().unwrap()));
        assert_eq!(
            fs::read_to_string(workspace.runtime_job.join(REGISTRY_CA)).unwrap(),
            "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----"
        );
        assert!(!workspace.cargo_home.join(REGISTRY_CA).exists());
        assert!(!tree_contains(&workspace.state_job, b"secret-token"));
        for name in [CARGO_CONFIG, CARGO_CREDENTIALS, REGISTRY_CA] {
            assert_regular_owned_mode(
                &workspace.runtime_job.join(name),
                DirectoryOwner::of(&account),
                0o600,
            );
        }
        workspace.remove_runtime_workspace().unwrap();
        assert!(!workspace.runtime_job.exists());
        assert!(!workspace.cargo_home.join(CARGO_CONFIG).exists());
        workspace.cleanup().unwrap();
    }

    #[test]
    fn systemd_runtime_workspace_is_scrubbed_without_removing_its_mount_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let boundary_owner = DirectoryOwner::of(&account);
        let state_root = temporary.path().join("state");
        let runtime_root = temporary.path().join("runtime");
        WorkspaceRoot::ensure(&state_root, boundary_owner).unwrap();
        let runtime = WorkspaceRoot::ensure(&runtime_root, boundary_owner).unwrap();
        let name = "a-0123456789abcdef0123456789abcdef";
        runtime.create_job(name, ExistingJob::Reject).unwrap();
        runtime.secure_job(name).unwrap();

        let mut workspace = PreparedUserInstall::prepare_at(
            &state_root,
            &runtime_root,
            "a",
            JobId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            account.clone(),
            boundary_owner,
            RuntimeManagement::Systemd,
        )
        .unwrap();
        workspace
            .populate_runtime_configuration(&CargoRegistry::CratesIo, &account)
            .unwrap();
        assert!(workspace.runtime_job.join(CARGO_CONFIG).is_file());
        assert_owned_mode(&workspace.runtime_job, boundary_owner, 0o711);
        workspace.cleanup().unwrap();
        assert_owned_mode(&workspace.runtime_job, boundary_owner, 0o711);
        assert!(
            fs::read_dir(&workspace.runtime_job)
                .unwrap()
                .next()
                .is_none()
        );
        fs::remove_dir(&workspace.runtime_job).unwrap();
    }

    #[test]
    fn orphan_cleanup_preserves_current_and_foreign_systemd_runtime_workspaces() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let boundary_owner = DirectoryOwner::of(&account);
        let state_root = temporary.path().join("state");
        let runtime_root = temporary.path().join("runtime");
        WorkspaceRoot::ensure(&state_root, boundary_owner).unwrap();
        let runtime = WorkspaceRoot::ensure(&runtime_root, boundary_owner).unwrap();
        let name = "a-0123456789abcdef0123456789abcdef";
        runtime.create_job(name, ExistingJob::Reject).unwrap();
        runtime.secure_job(name).unwrap();
        let foreign = "b-11111111111111111111111111111111";
        runtime.create_job(foreign, ExistingJob::Reject).unwrap();
        runtime.secure_job(foreign).unwrap();

        remove_orphaned_user_installations_at(
            &state_root,
            &runtime_root,
            boundary_owner,
            Some(name),
        )
        .unwrap();

        assert_owned_mode(&runtime_root.join(name), boundary_owner, 0o711);
        assert_owned_mode(&runtime_root.join(foreign), boundary_owner, 0o711);
        fs::remove_dir(runtime_root.join(name)).unwrap();
        fs::remove_dir(runtime_root.join(foreign)).unwrap();
    }

    #[test]
    fn a_runtime_workspace_removed_by_systemd_during_validation_is_absent() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let boundary_owner = DirectoryOwner::of(&account);
        let runtime =
            WorkspaceRoot::ensure(&temporary.path().join("runtime"), boundary_owner).unwrap();

        assert!(
            validate_runtime_workspace(
                &runtime.directory,
                "a-0123456789abcdef0123456789abcdef".to_string(),
                boundary_owner,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn volatile_file_disappearance_during_systemd_teardown_is_successful() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = File::open(temporary.path()).unwrap();

        assert!(!require_regular_file_if_present(&directory, CARGO_CONFIG).unwrap());
        scrub_runtime_entries(&directory, vec![CARGO_CONFIG.to_string()]).unwrap();
    }

    #[test]
    fn replaced_cargo_directory_cannot_redirect_root_link_creation() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account.clone());
        let outside = temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), b"preserve").unwrap();
        fs::rename(
            &workspace.cargo_home,
            workspace.state_job.join("original-cargo"),
        )
        .unwrap();
        symlink(&outside, &workspace.cargo_home).unwrap();

        assert!(
            workspace
                .populate_runtime_configuration(&CargoRegistry::CratesIo, &account)
                .is_err()
        );
        assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"preserve");
        assert!(!outside.join(CARGO_CONFIG).exists());

        fs::remove_file(&workspace.cargo_home).unwrap();
        fs::rename(
            workspace.state_job.join("original-cargo"),
            &workspace.cargo_home,
        )
        .unwrap();
        workspace.cleanup().unwrap();
    }

    #[test]
    fn workspace_boundaries_are_exact_and_orphans_are_cleaned_from_both_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account.clone());
        assert_owned_mode(
            &workspace.state_root,
            workspace.test_boundary_owner(),
            0o711,
        );
        assert_owned_mode(
            &workspace.runtime_root,
            workspace.test_boundary_owner(),
            0o711,
        );
        assert_owned_mode(&workspace.state_job, workspace.test_boundary_owner(), 0o711);
        assert_owned_mode(
            &workspace.runtime_job,
            workspace.test_boundary_owner(),
            0o711,
        );
        assert_owned_mode(&workspace.cargo_home, DirectoryOwner::of(&account), 0o700);
        assert_owned_mode(&workspace.target, DirectoryOwner::of(&account), 0o700);
        let state_root = workspace.state_root.clone();
        let runtime_root = workspace.runtime_root.clone();
        let boundary_owner = workspace.test_boundary_owner();
        workspace.state_present = false;
        workspace.runtime_present = false;
        drop(workspace);

        remove_orphaned_user_installations_at(&state_root, &runtime_root, boundary_owner, None)
            .unwrap();
        assert!(fs::read_dir(state_root).unwrap().next().is_none());
        assert!(fs::read_dir(runtime_root).unwrap().next().is_none());
    }

    #[test]
    fn busy_systemd_mount_defers_its_matching_persistent_workspace() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account.clone());
        workspace
            .populate_runtime_configuration(&CargoRegistry::CratesIo, &account)
            .unwrap();
        let state_root = workspace.state_root.clone();
        let runtime_root = workspace.runtime_root.clone();
        let state_job = workspace.state_job.clone();
        let runtime_job = workspace.runtime_job.clone();
        let boundary_owner = workspace.test_boundary_owner();
        assert_owned_mode(&runtime_job, boundary_owner, 0o711);
        workspace.state_present = false;
        workspace.runtime_present = false;
        drop(workspace);

        assert_eq!(
            remove_orphaned_user_installations_at_with(
                &state_root,
                &runtime_root,
                boundary_owner,
                None,
                |path| {
                    assert_eq!(path, runtime_job);
                    Err(std::io::ErrorKind::ResourceBusy.into())
                },
            )
            .unwrap(),
            CleanupDisposition::Busy
        );
        assert!(state_job.is_dir());
        assert!(runtime_job.is_dir());
        let runtime_entries = fs::read_dir(&runtime_job)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(runtime_entries.is_empty(), "{runtime_entries:?}");

        assert_eq!(
            remove_orphaned_user_installations_at(
                &state_root,
                &runtime_root,
                boundary_owner,
                None,
            )
            .unwrap(),
            CleanupDisposition::Complete
        );
        assert!(fs::read_dir(state_root).unwrap().next().is_none());
        assert!(fs::read_dir(runtime_root).unwrap().next().is_none());
    }

    #[test]
    fn interrupted_roots_and_state_jobs_recover_while_runtime_only_jobs_are_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let boundary_owner = test_boundary_owner(&account);
        let state_root = temporary.path().join("state");
        let runtime_root = temporary.path().join("runtime");
        create_owned_directory(&state_root, boundary_owner, 0o500);
        create_owned_directory(&runtime_root, boundary_owner, 0o500);

        let mut workspace = PreparedUserInstall::prepare_at(
            &state_root,
            &runtime_root,
            "a",
            JobId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            account,
            boundary_owner,
            RuntimeManagement::Process,
        )
        .unwrap();
        assert_owned_mode(&state_root, boundary_owner, 0o711);
        assert_owned_mode(&runtime_root, boundary_owner, 0o711);
        workspace.cleanup().unwrap();

        let interrupted_name = "a-00000000000000000000000000000000";
        create_owned_directory(&state_root.join(interrupted_name), boundary_owner, 0o700);
        create_owned_directory(&runtime_root.join(interrupted_name), boundary_owner, 0o500);
        let runtime_job = runtime_root.join("a-11111111111111111111111111111111");
        create_owned_directory(&runtime_job, boundary_owner, 0o500);
        remove_orphaned_user_installations_at(&state_root, &runtime_root, boundary_owner, None)
            .unwrap();
        assert!(fs::read_dir(state_root).unwrap().next().is_none());
        assert!(!runtime_root.join(interrupted_name).exists());
        assert_owned_mode(&runtime_job, boundary_owner, 0o500);
        fs::remove_dir(runtime_job).unwrap();
    }

    #[test]
    fn workspace_roots_reject_broad_modes_wrong_owners_and_nonempty_creation_states() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let boundary_owner = test_boundary_owner(&account);

        let broad = temporary.path().join("broad");
        create_owned_directory(&broad, boundary_owner, 0o755);
        assert!(WorkspaceRoot::ensure(&broad, boundary_owner).is_err());
        assert_owned_mode(&broad, boundary_owner, 0o755);

        let wrong_owner = temporary.path().join("wrong-owner");
        create_owned_directory(&wrong_owner, boundary_owner, 0o711);
        let nonexistent_owner = DirectoryOwner {
            uid: boundary_owner.uid.wrapping_add(1),
            gid: boundary_owner.gid,
        };
        assert!(WorkspaceRoot::ensure(&wrong_owner, nonexistent_owner).is_err());

        let nonempty = temporary.path().join("nonempty");
        create_owned_directory(&nonempty, boundary_owner, 0o700);
        fs::write(nonempty.join("unexpected"), b"preserve").unwrap();
        fs::set_permissions(&nonempty, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(WorkspaceRoot::ensure(&nonempty, boundary_owner).is_err());
        assert_eq!(fs::read(nonempty.join("unexpected")).unwrap(), b"preserve");
        assert_owned_mode(&nonempty, boundary_owner, 0o500);
    }

    #[test]
    fn orphan_cleanup_handles_a_workspace_present_in_only_one_root() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account);
        workspace.remove_runtime_workspace().unwrap();
        let state_root = workspace.state_root.clone();
        let runtime_root = workspace.runtime_root.clone();
        let boundary_owner = workspace.test_boundary_owner();
        workspace.state_present = false;
        drop(workspace);

        remove_orphaned_user_installations_at(&state_root, &runtime_root, boundary_owner, None)
            .unwrap();
        assert!(fs::read_dir(state_root).unwrap().next().is_none());
        assert!(fs::read_dir(runtime_root).unwrap().next().is_none());
    }

    #[test]
    fn legacy_runtime_workspace_admission_needs_no_nss_record() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let actual_owner = DirectoryOwner::of(&account);
        let root = WorkspaceRoot::ensure(&temporary.path().join("runtime"), actual_owner).unwrap();
        let name = "a-0123456789abcdef0123456789abcdef";
        create_owned_directory(&root.path.join(name), actual_owner, 0o700);
        fs::write(
            root.path.join(name).join("unvalidated-cargo-cache"),
            b"legacy",
        )
        .unwrap();
        let boundary_owner = DirectoryOwner {
            uid: actual_owner.uid.wrapping_add(1),
            gid: actual_owner.gid,
        };

        assert!(
            validate_workspace_directory(
                &root.directory,
                name,
                boundary_owner,
                WorkspaceKind::Runtime,
            )
            .unwrap()
        );
        assert!(
            validate_workspace_directory(
                &root.directory,
                name,
                boundary_owner,
                WorkspaceKind::State,
            )
            .is_err()
        );
    }

    #[test]
    fn cleanup_accepts_mutable_metadata_below_the_immutable_job_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account);
        fs::set_permissions(&workspace.cargo_home, fs::Permissions::from_mode(0o755)).unwrap();

        workspace.cleanup().unwrap();
        assert!(!workspace.state_job.exists());
        assert!(!workspace.runtime_job.exists());
    }

    #[test]
    fn cleanup_removes_volatile_state_first_and_preserves_persistent_state_on_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account);
        let unexpected = workspace.runtime_job.join("unexpected");
        fs::write(&unexpected, b"preserve").unwrap();

        assert!(workspace.cleanup().is_err());
        assert!(workspace.runtime_job.exists());
        assert!(workspace.state_job.exists());
        fs::remove_file(unexpected).unwrap();
        workspace.cleanup().unwrap();
    }

    #[test]
    fn orphan_validation_rejects_symlinks_and_wrong_modes_before_deleting_anything() {
        let temporary = tempfile::tempdir().unwrap();
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let mut workspace = test_workspace(temporary.path(), account);
        let sentinel = temporary.path().join("sentinel");
        fs::write(&sentinel, b"preserve").unwrap();
        symlink(
            &sentinel,
            workspace
                .runtime_root
                .join("a-00000000000000000000000000000000"),
        )
        .unwrap();
        assert!(
            remove_orphaned_user_installations_at(
                &workspace.state_root,
                &workspace.runtime_root,
                workspace.test_boundary_owner(),
                None,
            )
            .is_err()
        );
        assert!(workspace.state_job.exists());
        assert_eq!(fs::read(sentinel).unwrap(), b"preserve");
        fs::remove_file(
            workspace
                .runtime_root
                .join("a-00000000000000000000000000000000"),
        )
        .unwrap();
        fs::set_permissions(&workspace.state_job, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            remove_orphaned_user_installations_at(
                &workspace.state_root,
                &workspace.runtime_root,
                workspace.test_boundary_owner(),
                None,
            )
            .is_err()
        );
        fs::set_permissions(&workspace.state_job, fs::Permissions::from_mode(0o711)).unwrap();
        workspace.cleanup().unwrap();
    }

    fn test_context(account: &UnixAccount) -> UserInstallContext {
        UserInstallContext {
            account_name: account.name.clone(),
            uid: account.uid,
            gid: account.gid,
            home: account.home.clone(),
            cargo_home: account.home.join(".cargo"),
            rustup_home: account.home.join(".rustup"),
            cargo: account.home.join(".cargo/bin/cargo"),
            rustup: account.home.join(".cargo/bin/rustup"),
            installed_binary: account.home.join(".cargo/bin/a"),
        }
    }

    fn test_workspace(base: &Path, account: UnixAccount) -> PreparedUserInstall {
        let boundary_owner = test_boundary_owner(&account);
        PreparedUserInstall::prepare_at(
            &base.join("state"),
            &base.join("runtime"),
            "a",
            JobId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            account,
            boundary_owner,
            RuntimeManagement::Process,
        )
        .unwrap()
    }

    fn test_boundary_owner(account: &UnixAccount) -> DirectoryOwner {
        DirectoryOwner::of(account)
    }

    impl PreparedUserInstall {
        fn test_boundary_owner(&self) -> DirectoryOwner {
            self.boundary_owner
        }
    }

    fn assert_owned_mode(path: &Path, owner: DirectoryOwner, mode: u32) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(DirectoryIdentity::from_metadata(&metadata).is_owned_directory(owner, mode));
    }

    fn create_owned_directory(path: &Path, owner: DirectoryOwner, mode: u32) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        assert_owned_mode(path, owner, mode);
    }

    fn assert_regular_owned_mode(path: &Path, owner: DirectoryOwner, mode: u32) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.uid(), owner.uid);
        assert_eq!(metadata.gid(), owner.gid);
        assert_eq!(metadata.mode() & 0o777, mode);
    }

    fn tree_contains(path: &Path, needle: &[u8]) -> bool {
        fs::read_dir(path).unwrap().any(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            if metadata.file_type().is_symlink() {
                false
            } else if metadata.is_dir() {
                tree_contains(&entry.path(), needle)
            } else {
                fs::read(entry.path())
                    .unwrap()
                    .windows(needle.len())
                    .any(|bytes| bytes == needle)
            }
        })
    }
}
