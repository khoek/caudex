use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{
    AtFlags, Gid, Mode, OFlags, RawDir, Uid, chmodat, fchmod, fchown, mkdirat, openat,
};
use serde::{Deserialize, Serialize};

use super::product::is_valid_identifier;
use super::{JobId, PeerCredentials};

pub const BUILD_USER: &str = "capulus-build";
pub const BUILD_GROUP: &str = "capulus-build";
pub const BUILD_HOME: &str = "/var/lib/capulus-build";
pub const BUILD_CARGO_TOOLS_HOME: &str = "/var/lib/capulus-build/cargo-tools";
pub const BUILD_RUSTUP_HOME: &str = "/var/lib/capulus-build/rustup";
pub const BUILD_CACHE_HOME: &str = "/var/lib/capulus-build/cache";
pub const BUILD_TARGET_HOME: &str = "/var/lib/capulus-build/target";
pub const BUILD_JOBS_HOME: &str = "/var/lib/capulus-build/jobs";
const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnixAccount {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

impl UnixAccount {
    pub fn by_uid(uid: u32) -> Result<Self> {
        lookup_passwd(uid, None)?.ok_or_else(|| anyhow!("no NSS account exists for UID {uid}"))
    }

    pub fn by_name(name: &str) -> Result<Option<Self>> {
        let name = CString::new(name).context("NSS account name contains a NUL byte")?;
        lookup_passwd(0, Some(&name))
    }

    pub fn validate_interactive(&self) -> Result<()> {
        if self.uid == 0 || self.uid < login_uid_min()? {
            bail!("UID {} is a root or system account", self.uid);
        }
        validate_absolute_normal_path(&self.home)?;
        if !self.home.is_dir() {
            bail!("NSS home directory does not exist: {}", self.home.display());
        }
        let shell = self.shell.file_name().and_then(|name| name.to_str());
        if matches!(shell, Some("nologin" | "false")) {
            bail!("NSS account {} is not an interactive login", self.name);
        }
        Ok(())
    }

    pub(super) fn command(
        &self,
        program: impl AsRef<OsStr>,
        supplementary_groups: SupplementaryGroups,
    ) -> Command {
        let mut command = Command::new("/usr/bin/setpriv");
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .env("SHELL", &self.shell)
            .env("PATH", SYSTEM_PATH)
            .env("LANG", "C.UTF-8")
            .args([
                format!("--reuid={}", self.uid),
                format!("--regid={}", self.gid),
                supplementary_groups.setpriv_argument().to_string(),
                "--".to_string(),
            ])
            .arg(program)
            .current_dir(&self.home)
            .stdin(Stdio::null());
        command
    }

    pub(super) fn create_file(&self, path: &Path, mode: u32) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        fchown(
            &file,
            Some(Uid::from_raw(self.uid)),
            Some(Gid::from_raw(self.gid)),
        )
        .map_err(std::io::Error::from)
        .with_context(|| format!("failed to set ownership on {}", path.display()))?;
        fchmod(&file, Mode::from_raw_mode(mode))
            .map_err(std::io::Error::from)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
        Ok(file)
    }

    pub(super) fn write_file(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let mut file = self.create_file(path, mode)?;
        file.write_all(bytes)?;
        file.sync_all().map_err(Into::into)
    }

    fn validate_build_account(&self) -> Result<()> {
        if self.uid == 0
            || self.gid == 0
            || self.uid >= login_id_min("UID_MIN")?
            || self.gid >= login_id_min("GID_MIN")?
        {
            bail!("{BUILD_USER} must be a non-root system account and group");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SupplementaryGroups {
    Initialize,
    Clear,
}

impl SupplementaryGroups {
    fn setpriv_argument(self) -> &'static str {
        match self {
            Self::Initialize => "--init-groups",
            Self::Clear => "--clear-groups",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildAccount {
    pub account: UnixAccount,
    pub cargo_tools_home: PathBuf,
    pub rustup_home: PathBuf,
    pub cache_home: PathBuf,
    pub target_home: PathBuf,
    pub jobs_home: PathBuf,
}

impl BuildAccount {
    fn home(&self) -> Result<&Path> {
        let home = self
            .cargo_tools_home
            .parent()
            .ok_or_else(|| anyhow!("Capulus cargo-tools directory has no parent"))?;
        validate_absolute_normal_path(home)?;
        for (path, name) in [
            (&self.cargo_tools_home, "cargo-tools"),
            (&self.rustup_home, "rustup"),
            (&self.cache_home, "cache"),
            (&self.target_home, "target"),
            (&self.jobs_home, "jobs"),
        ] {
            if path.parent() != Some(home) || path.file_name() != Some(OsStr::new(name)) {
                bail!("Capulus build paths do not share the canonical home layout");
            }
        }
        Ok(home)
    }

    fn private_directories(&self) -> [&Path; 4] {
        [
            &self.cargo_tools_home,
            &self.rustup_home,
            &self.cache_home,
            &self.target_home,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectoryOwner {
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl DirectoryOwner {
    pub(super) const ROOT: Self = Self { uid: 0, gid: 0 };

    pub(super) fn of(account: &UnixAccount) -> Self {
        Self {
            uid: account.uid,
            gid: account.gid,
        }
    }
}

impl BuildAccount {
    pub fn ensure() -> Result<Self> {
        require_root()?;
        if UnixAccount::by_name(BUILD_USER)?.is_none() {
            ensure_build_group()?;
            checked_command(
                Command::new("/usr/sbin/useradd").args([
                    "--system",
                    "--gid",
                    BUILD_GROUP,
                    "--home-dir",
                    BUILD_HOME,
                    "--no-create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    BUILD_USER,
                ]),
                "create shared Capulus build account",
            )?;
        }
        let account = UnixAccount::by_name(BUILD_USER)?
            .ok_or_else(|| anyhow!("shared Capulus build account was not created"))?;
        account.validate_build_account()?;
        if account.home != Path::new(BUILD_HOME)
            || account.shell != Path::new("/usr/sbin/nologin")
            || group_name(account.gid)?.as_deref() != Some(BUILD_GROUP)
        {
            bail!(
                "existing {BUILD_USER} account does not match the required home, shell, and primary group"
            );
        }
        let build = Self {
            account,
            cargo_tools_home: PathBuf::from(BUILD_CARGO_TOOLS_HOME),
            rustup_home: PathBuf::from(BUILD_RUSTUP_HOME),
            cache_home: PathBuf::from(BUILD_CACHE_HOME),
            target_home: PathBuf::from(BUILD_TARGET_HOME),
            jobs_home: PathBuf::from(BUILD_JOBS_HOME),
        };
        build.ensure_layout_with(DirectoryOwner::ROOT)?;
        Ok(build)
    }

    fn ensure_layout_with(&self, boundary_owner: DirectoryOwner) -> Result<()> {
        let home_path = self.home()?;
        let (home, state) = self.ensure_home_boundary(home_path, boundary_owner)?;
        self.validate_home_entries(home_path, state, boundary_owner)?;
        for path in self.private_directories() {
            self.ensure_private_directory(path, boundary_owner)?;
        }
        self.ensure_jobs_directory(state, boundary_owner)?;
        home.sync_all()?;
        if state == BoundaryState::OwnershipSecured {
            set_directory_mode_and_sync(
                &home,
                0o711,
                "failed to finish the Capulus build-home migration",
            )?;
        }
        Ok(())
    }

    fn ensure_home_boundary(
        &self,
        path: &Path,
        boundary_owner: DirectoryOwner,
    ) -> Result<(File, BoundaryState)> {
        let (directory, initializing) =
            open_or_create_restricted_directory(path, boundary_owner)
                .context("failed to open or create the Capulus build home")?;
        let state = if initializing {
            set_directory_owner_and_sync(
                &directory,
                boundary_owner,
                "failed to secure ownership of the Capulus build home",
            )?;
            open_real_directory(
                path.parent()
                    .expect("the build home has a validated parent"),
            )?
            .sync_all()?;
            BoundaryState::OwnershipSecured
        } else {
            match BoundaryState::classify(
                DirectoryIdentity::from_metadata(&directory.metadata()?),
                DirectoryOwner::of(&self.account),
                boundary_owner,
            )? {
                BoundaryState::PriorLayout => {
                    set_directory_owner_and_sync(
                        &directory,
                        boundary_owner,
                        "failed to secure ownership of the Capulus build home",
                    )?;
                    BoundaryState::OwnershipSecured
                }
                state => state,
            }
        };
        Ok((directory, state))
    }

    fn validate_home_entries(
        &self,
        home: &Path,
        home_state: BoundaryState,
        boundary_owner: DirectoryOwner,
    ) -> Result<()> {
        for entry in fs::read_dir(home).context("failed to inspect the Capulus build home")? {
            let entry = entry?;
            let name = entry.file_name();
            let identity = DirectoryIdentity::from_metadata(&fs::symlink_metadata(entry.path())?);
            if self
                .private_directories()
                .iter()
                .any(|path| path.file_name() == Some(name.as_os_str()))
            {
                match PrivateDirectoryState::classify(
                    identity,
                    DirectoryOwner::of(&self.account),
                    boundary_owner,
                )
                .with_context(|| {
                    format!(
                        "Capulus private build directory has invalid type, ownership, or mode: {}",
                        entry.path().display()
                    )
                })? {
                    PrivateDirectoryState::Current => {}
                    PrivateDirectoryState::CreationInterrupted => {
                        require_empty_directory(
                            &open_real_directory(&entry.path())?,
                            &entry.path(),
                        )?;
                    }
                }
            } else if self.jobs_home.file_name() == Some(name.as_os_str()) {
                let creation_restricted = identity.is_restricted_creation(boundary_owner);
                let jobs_state = if creation_restricted {
                    BoundaryState::OwnershipSecured
                } else {
                    BoundaryState::classify(
                        identity,
                        DirectoryOwner::of(&self.account),
                        boundary_owner,
                    )
                    .with_context(|| {
                        format!(
                            "Capulus build jobs directory has invalid type, ownership, or mode: {}",
                            entry.path().display()
                        )
                    })?
                };
                if home_state == BoundaryState::Current && jobs_state == BoundaryState::PriorLayout
                {
                    bail!(
                        "Capulus current build home contains a prior-layout jobs directory: {}",
                        entry.path().display()
                    );
                }
                if creation_restricted
                    || (home_state == BoundaryState::Current
                        && jobs_state == BoundaryState::OwnershipSecured)
                {
                    require_empty_directory(&open_real_directory(&entry.path())?, &entry.path())?;
                }
            } else {
                bail!(
                    "Capulus build home contains an unexpected top-level entry: {:?}",
                    name
                );
            }
        }
        Ok(())
    }

    fn ensure_private_directory(&self, path: &Path, boundary_owner: DirectoryOwner) -> Result<()> {
        let (directory, initializing) =
            open_or_create_restricted_directory(path, boundary_owner)
                .with_context(|| format!("failed to open or create {}", path.display()))?;
        let state = if initializing {
            PrivateDirectoryState::CreationInterrupted
        } else {
            PrivateDirectoryState::classify(
                DirectoryIdentity::from_metadata(&directory.metadata()?),
                DirectoryOwner::of(&self.account),
                boundary_owner,
            )?
        };
        match state {
            PrivateDirectoryState::Current => Ok(()),
            PrivateDirectoryState::CreationInterrupted => {
                require_empty_directory(&directory, path)?;
                set_directory_owner_and_sync(
                    &directory,
                    DirectoryOwner::of(&self.account),
                    &format!("failed to finish creating {}", path.display()),
                )
            }
        }
    }

    fn ensure_jobs_directory(
        &self,
        home_state: BoundaryState,
        boundary_owner: DirectoryOwner,
    ) -> Result<()> {
        let (directory, initializing) =
            open_or_create_restricted_directory(&self.jobs_home, boundary_owner)
                .context("failed to open or create the Capulus build jobs directory")?;
        let mut state = if initializing {
            set_directory_owner_and_sync(
                &directory,
                boundary_owner,
                "failed to secure ownership of the Capulus build jobs directory",
            )?;
            BoundaryState::OwnershipSecured
        } else {
            BoundaryState::classify(
                DirectoryIdentity::from_metadata(&directory.metadata()?),
                DirectoryOwner::of(&self.account),
                boundary_owner,
            )?
        };
        if home_state == BoundaryState::Current && state == BoundaryState::PriorLayout {
            bail!("Capulus current build home contains a prior-layout jobs directory");
        }
        if home_state == BoundaryState::Current && state == BoundaryState::OwnershipSecured {
            require_empty_directory(&directory, &self.jobs_home)?;
        }
        if state == BoundaryState::PriorLayout {
            set_directory_owner_and_sync(
                &directory,
                boundary_owner,
                "failed to secure ownership of the Capulus build jobs directory",
            )?;
            state = BoundaryState::OwnershipSecured;
        }
        if state == BoundaryState::OwnershipSecured {
            set_directory_mode_and_sync(
                &directory,
                0o711,
                "failed to finish the Capulus build-jobs migration",
            )?;
        }
        Ok(())
    }

    pub fn cargo(&self) -> PathBuf {
        self.cargo_tools_home.join("bin/cargo")
    }

    pub fn rustup(&self) -> PathBuf {
        self.cargo_tools_home.join("bin/rustup")
    }

    pub(super) fn reclaim_target_home(&self) -> Result<()> {
        require_root()?;
        self.reclaim_target_home_with(DirectoryOwner::ROOT)
    }

    fn reclaim_target_home_with(&self, boundary_owner: DirectoryOwner) -> Result<()> {
        let home_path = self.home()?;
        let home = open_real_directory(home_path)?;
        if !DirectoryIdentity::from_metadata(&home.metadata()?)
            .is_owned_directory(boundary_owner, 0o711)
        {
            bail!("Capulus cannot reclaim a target outside its current build-home boundary");
        }
        let target = open_directory_at(&home, OsStr::new("target"))?;
        if !DirectoryIdentity::from_metadata(&target.metadata()?)
            .is_owned_directory(DirectoryOwner::of(&self.account), 0o700)
        {
            bail!("Capulus build target ownership or mode changed before reclamation");
        }
        fs::remove_dir_all(&self.target_home)
            .context("failed to reclaim the shared build target")?;
        home.sync_all()?;
        self.ensure_private_directory(&self.target_home, boundary_owner)?;
        home.sync_all()?;
        let target = open_directory_at(&home, OsStr::new("target"))?;
        if !DirectoryIdentity::from_metadata(&target.metadata()?)
            .is_owned_directory(DirectoryOwner::of(&self.account), 0o700)
        {
            bail!("Capulus did not recreate its shared build target securely");
        }
        Ok(())
    }

    pub(super) fn command(&self, program: impl AsRef<OsStr>, cargo_home: &Path) -> Command {
        let mut path = self.cargo_tools_home.join("bin").into_os_string();
        path.push(":");
        path.push(SYSTEM_PATH);
        let mut command = self.account.command(program, SupplementaryGroups::Clear);
        command
            .env("CARGO_HOME", cargo_home)
            .env("RUSTUP_HOME", &self.rustup_home)
            .env("CARGO_TARGET_DIR", &self.target_home)
            .env("PATH", path);
        command
    }

    pub(super) fn remove_orphaned_jobs(&self) -> Result<()> {
        for entry in fs::read_dir(&self.jobs_home)
            .with_context(|| format!("failed to inspect {}", self.jobs_home.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| anyhow!("Capulus build job has a non-UTF-8 name"))?;
            let Some((product, job)) = name.rsplit_once('-') else {
                bail!("Capulus build job has an invalid name: {name:?}");
            };
            if !is_valid_identifier(product) || JobId::parse(job).is_err() {
                bail!("Capulus build job has an invalid name: {name:?}");
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_dir()
                || metadata.uid() != self.account.uid
                || metadata.gid() != self.account.gid
            {
                bail!("Capulus build job is not an owned real directory: {name:?}");
            }
            fs::remove_dir_all(entry.path())
                .with_context(|| format!("failed to remove orphaned Capulus build job {name}"))?;
        }
        File::open(&self.jobs_home)?.sync_all().map_err(Into::into)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectoryIdentity {
    is_directory: bool,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl DirectoryIdentity {
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            is_directory: metadata.file_type().is_dir(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.permissions().mode() & 0o7777,
        }
    }

    pub(super) fn is_owned_directory(self, owner: DirectoryOwner, mode: u32) -> bool {
        self.is_directory_with_mode(mode) && self.uid == owner.uid && self.gid == owner.gid
    }

    pub(super) fn is_directory_with_mode(self, mode: u32) -> bool {
        self.is_directory && self.mode == mode
    }

    pub(super) fn is_directory_owned_by_other_uid(self, owner: DirectoryOwner, mode: u32) -> bool {
        self.is_directory_with_mode(mode) && self.uid != owner.uid
    }

    fn is_restricted_creation(self, owner: DirectoryOwner) -> bool {
        self.is_restricted(owner, 0o700)
    }

    pub(super) fn is_restricted(self, owner: DirectoryOwner, mode: u32) -> bool {
        self.is_directory
            && self.uid == owner.uid
            && self.gid == owner.gid
            && self.mode != mode
            && self.mode & !mode == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundaryState {
    Current,
    PriorLayout,
    OwnershipSecured,
}

impl BoundaryState {
    fn classify(
        identity: DirectoryIdentity,
        prior_owner: DirectoryOwner,
        current_owner: DirectoryOwner,
    ) -> Result<Self> {
        if identity.is_owned_directory(current_owner, 0o711) {
            Ok(Self::Current)
        } else if identity.is_owned_directory(prior_owner, 0o700) {
            Ok(Self::PriorLayout)
        } else if identity.is_owned_directory(current_owner, 0o700) {
            Ok(Self::OwnershipSecured)
        } else {
            bail!(
                "Capulus build boundary is neither current, an exact prior layout, nor an interrupted migration"
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrivateDirectoryState {
    Current,
    CreationInterrupted,
}

impl PrivateDirectoryState {
    fn classify(
        identity: DirectoryIdentity,
        current_owner: DirectoryOwner,
        boundary_owner: DirectoryOwner,
    ) -> Result<Self> {
        if identity.is_owned_directory(current_owner, 0o700) {
            Ok(Self::Current)
        } else if identity.is_owned_directory(boundary_owner, 0o700)
            || identity.is_restricted_creation(boundary_owner)
        {
            Ok(Self::CreationInterrupted)
        } else {
            bail!("Capulus private build directory is neither current nor an interrupted creation")
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserInstallContext {
    pub account_name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub cargo_home: PathBuf,
    pub rustup_home: PathBuf,
    pub cargo: PathBuf,
    pub rustup: PathBuf,
    pub installed_binary: PathBuf,
}

impl UserInstallContext {
    pub fn capture(peer: PeerCredentials, binary_name: &str) -> Result<Option<Self>> {
        let account = UnixAccount::by_uid(peer.uid)?;
        account.validate_interactive()?;
        validate_peer_process(peer, &account)?;
        let environment = read_process_environment(peer.pid)?;
        let cargo_home = environment_path(&environment, b"CARGO_HOME")
            .unwrap_or_else(|| account.home.join(".cargo"));
        let rustup_home = environment_path(&environment, b"RUSTUP_HOME")
            .unwrap_or_else(|| account.home.join(".rustup"));
        for path in [&cargo_home, &rustup_home] {
            validate_absolute_normal_path(path)?;
            if !path.starts_with(&account.home) {
                bail!(
                    "caller Cargo/Rustup path is outside its NSS home: {}",
                    path.display()
                );
            }
        }
        let installed_binary = cargo_home.join("bin").join(binary_name);
        if !owned_executable_regular_file(&installed_binary, account.uid)? {
            return Ok(None);
        }
        let cargo = cargo_home.join("bin/cargo");
        let rustup = cargo_home.join("bin/rustup");
        if !owned_executable_regular_file(&rustup, account.uid)? {
            bail!(
                "existing user CLI has no valid Rustup executable at {}",
                rustup.display()
            );
        }
        if !owned_cargo_executable(&cargo, &rustup, account.uid)? {
            bail!(
                "existing user CLI has no valid Cargo executable at {}",
                cargo.display()
            );
        }
        Ok(Some(Self {
            account_name: account.name,
            uid: account.uid,
            gid: account.gid,
            home: account.home,
            cargo_home,
            rustup_home,
            cargo,
            rustup,
            installed_binary,
        }))
    }

    pub fn revalidate(&self, binary_name: &str) -> Result<UnixAccount> {
        let account = UnixAccount::by_uid(self.uid)?;
        account.validate_interactive()?;
        if account.name != self.account_name
            || account.gid != self.gid
            || account.home != self.home
            || self.installed_binary != self.cargo_home.join("bin").join(binary_name)
        {
            bail!("requesting user's NSS or Cargo identity changed during redeploy");
        }
        for path in [
            &self.cargo_home,
            &self.rustup_home,
            &self.cargo,
            &self.rustup,
            &self.installed_binary,
        ] {
            validate_absolute_normal_path(path)?;
            if !path.starts_with(&self.home) {
                bail!("requesting user's validated path escaped its NSS home");
            }
        }
        if !owned_executable_regular_file(&self.rustup, self.uid)?
            || !owned_cargo_executable(&self.cargo, &self.rustup, self.uid)?
            || !owned_executable_regular_file(&self.installed_binary, self.uid)?
        {
            bail!("requesting user's Cargo installation changed during redeploy");
        }
        Ok(account)
    }
}

pub(super) fn require_root() -> Result<()> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        bail!("managed-system operation requires root")
    }
}

fn ensure_build_group() -> Result<()> {
    if group_gid(BUILD_GROUP)?.is_some() {
        return Ok(());
    }
    checked_command(
        Command::new("/usr/sbin/groupadd").args(["--system", BUILD_GROUP]),
        "create shared Capulus build group",
    )
}

pub(super) fn ensure_owned_directory(path: &Path, account: &UnixAccount, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            bail!(
                "managed directory path is not a real directory: {}",
                path.display()
            )
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    let directory =
        open_real_directory(path).with_context(|| format!("failed to open {}", path.display()))?;
    fchown(
        &directory,
        Some(Uid::from_raw(account.uid)),
        Some(Gid::from_raw(account.gid)),
    )
    .map_err(std::io::Error::from)
    .with_context(|| format!("failed to chown {}", path.display()))?;
    fchmod(&directory, Mode::from_raw_mode(mode))
        .map_err(std::io::Error::from)
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    directory.sync_all().map_err(Into::into)
}

pub(super) fn ensure_root_directory(path: &Path, mode: u32) -> Result<()> {
    let (directory, initializing) =
        open_or_create_restricted_directory(path, DirectoryOwner::ROOT)?;
    let identity = DirectoryIdentity::from_metadata(&directory.metadata()?);
    if !identity.is_directory || identity.uid != 0 || identity.gid != 0 {
        bail!(
            "Capulus path is not a root-owned real directory: {}",
            path.display()
        );
    }
    fchmod(&directory, Mode::from_raw_mode(mode)).map_err(std::io::Error::from)?;
    directory.sync_all()?;
    if initializing {
        open_real_directory(path.parent().expect("managed directories have a parent"))?
            .sync_all()?;
    }
    Ok(())
}

fn open_or_create_restricted_directory(
    path: &Path,
    boundary_owner: DirectoryOwner,
) -> Result<(File, bool)> {
    open_or_create_restricted_directory_with(path, boundary_owner, || Ok(()))
}

fn open_or_create_restricted_directory_with(
    path: &Path,
    boundary_owner: DirectoryOwner,
    after_mkdir: impl FnOnce() -> Result<()>,
) -> Result<(File, bool)> {
    let parent_path = path
        .parent()
        .ok_or_else(|| anyhow!("managed directory has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("managed directory has no name: {}", path.display()))?;
    let parent = open_real_directory(parent_path)
        .with_context(|| format!("failed to open {}", parent_path.display()))?;
    let created = match mkdirat(&parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    if created {
        after_mkdir()?;
        chmodat(&parent, name, Mode::from_raw_mode(0o700), AtFlags::empty())
            .map_err(std::io::Error::from)
            .with_context(|| format!("failed to normalize {}", path.display()))?;
    }
    let directory = open_directory_at(&parent, name)
        .with_context(|| format!("failed to open {}", path.display()))?;
    if !created
        && DirectoryIdentity::from_metadata(&directory.metadata()?)
            .is_restricted_creation(boundary_owner)
    {
        require_empty_directory(&directory, path)?;
        set_directory_mode_and_sync(
            &directory,
            0o700,
            &format!("failed to resume creating {}", path.display()),
        )?;
        return Ok((directory, true));
    }
    Ok((directory, created))
}

pub(super) fn open_directory_at(parent: &File, name: &OsStr) -> Result<File> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| std::io::Error::from(error).into())
}

pub(super) fn open_file_at(parent: &File, name: &OsStr) -> Result<File> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| std::io::Error::from(error).into())
}

pub(super) fn require_empty_directory(directory: &File, path: &Path) -> Result<()> {
    let mut buffer = [MaybeUninit::uninit(); 4096];
    let mut entries = RawDir::new(directory, &mut buffer);
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(std::io::Error::from)?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            bail!(
                "interrupted Capulus directory creation is not empty: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub(super) fn open_real_directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(Into::into)
}

pub(super) fn set_directory_owner_and_sync(
    directory: &File,
    owner: DirectoryOwner,
    context: &str,
) -> Result<()> {
    fchown(
        directory,
        Some(Uid::from_raw(owner.uid)),
        Some(Gid::from_raw(owner.gid)),
    )
    .map_err(std::io::Error::from)
    .with_context(|| context.to_owned())?;
    directory.sync_all().map_err(Into::into)
}

pub(super) fn set_directory_mode_and_sync(
    directory: &File,
    mode: u32,
    context: &str,
) -> Result<()> {
    fchmod(directory, Mode::from_raw_mode(mode))
        .map_err(std::io::Error::from)
        .with_context(|| context.to_owned())?;
    directory.sync_all().map_err(Into::into)
}

fn checked_command(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to {action}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    });
    bail!("failed to {action}: {}", detail.trim())
}

fn lookup_passwd(uid: u32, name: Option<&CStr>) -> Result<Option<UnixAccount>> {
    let mut buffer = vec![0_u8; passwd_buffer_len()];
    let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer references live writable storage for the duration of the reentrant NSS
    // call. A non-null result points to `passwd`, whose string fields point into `buffer`.
    let status = unsafe {
        match name {
            Some(name) => libc::getpwnam_r(
                name.as_ptr(),
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ),
            None => libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ),
        }
    };
    if nss_not_found(status) {
        return Ok(None);
    }
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status)).context("NSS account lookup failed");
    }
    if result.is_null() {
        return Ok(None);
    }
    // SAFETY: successful NSS lookup initialized `passwd` and its fields remain backed by `buffer`.
    let passwd = unsafe { passwd.assume_init() };
    Ok(Some(UnixAccount {
        name: c_string(passwd.pw_name, "account name")?,
        uid: passwd.pw_uid,
        gid: passwd.pw_gid,
        home: PathBuf::from(c_os_string(passwd.pw_dir, "account home")?),
        shell: PathBuf::from(c_os_string(passwd.pw_shell, "account shell")?),
    }))
}

fn group_gid(name: &str) -> Result<Option<u32>> {
    let name = CString::new(name).context("NSS group name contains a NUL byte")?;
    lookup_group(Some(&name), 0).map(|group| group.map(|(gid, _)| gid))
}

fn group_name(gid: u32) -> Result<Option<String>> {
    lookup_group(None, gid).map(|group| group.map(|(_, name)| name))
}

fn lookup_group(name: Option<&CStr>, gid: u32) -> Result<Option<(u32, String)>> {
    let mut buffer = vec![0_u8; passwd_buffer_len()];
    let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
    let mut result = std::ptr::null_mut();
    // SAFETY: arguments reference valid storage for the complete reentrant NSS lookup.
    let status = unsafe {
        match name {
            Some(name) => libc::getgrnam_r(
                name.as_ptr(),
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ),
            None => libc::getgrgid_r(
                gid,
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            ),
        }
    };
    if nss_not_found(status) {
        return Ok(None);
    }
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status)).context("NSS group lookup failed");
    }
    if result.is_null() {
        return Ok(None);
    }
    // SAFETY: successful NSS lookup initialized `group` with strings backed by `buffer`.
    let group = unsafe { group.assume_init() };
    Ok(Some((group.gr_gid, c_string(group.gr_name, "group name")?)))
}

fn nss_not_found(status: libc::c_int) -> bool {
    matches!(status, libc::ENOENT | libc::ESRCH)
}

fn c_string(pointer: *const libc::c_char, field: &str) -> Result<String> {
    // SAFETY: NSS guarantees non-null, NUL-terminated string fields on a successful lookup.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    String::from_utf8(bytes.to_vec()).with_context(|| format!("NSS {field} is not UTF-8"))
}

fn c_os_string(pointer: *const libc::c_char, field: &str) -> Result<std::ffi::OsString> {
    if pointer.is_null() {
        bail!("NSS {field} is null");
    }
    // SAFETY: NSS guarantees NUL-terminated string fields on a successful lookup.
    Ok(std::ffi::OsStr::from_bytes(unsafe { CStr::from_ptr(pointer) }.to_bytes()).to_owned())
}

fn passwd_buffer_len() -> usize {
    // SAFETY: sysconf has no memory-safety preconditions.
    match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        value if value > 0 => value as usize,
        _ => 16 * 1024,
    }
}

fn login_uid_min() -> Result<u32> {
    login_id_min("UID_MIN")
}

fn login_id_min(name: &str) -> Result<u32> {
    let contents =
        fs::read_to_string("/etc/login.defs").context("failed to read /etc/login.defs")?;
    for line in contents.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let mut fields = line.split_whitespace();
        if fields.next() == Some(name) {
            return fields
                .next()
                .ok_or_else(|| anyhow!("{name} has no value in /etc/login.defs"))?
                .parse()
                .with_context(|| format!("{name} is invalid in /etc/login.defs"));
        }
    }
    bail!("/etc/login.defs does not define {name}")
}

fn validate_peer_process(peer: PeerCredentials, account: &UnixAccount) -> Result<()> {
    let status_path = Path::new("/proc").join(peer.pid.to_string()).join("status");
    let status = fs::read_to_string(&status_path).with_context(|| {
        format!(
            "failed to inspect management caller at {}",
            status_path.display()
        )
    })?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("caller process status omitted effective UID"))?
        .parse::<u32>()
        .context("caller process effective UID is invalid")?;
    if uid != peer.uid || account.uid != peer.uid {
        bail!("management caller process credentials changed during request capture");
    }
    Ok(())
}

fn read_process_environment(pid: u32) -> Result<Vec<u8>> {
    let path = Path::new("/proc").join(pid.to_string()).join("environ");
    fs::read(&path)
        .with_context(|| format!("failed to read caller environment at {}", path.display()))
}

fn environment_path(environment: &[u8], name: &[u8]) -> Option<PathBuf> {
    environment
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(name)?.strip_prefix(b"="))
        .filter(|value| !value.is_empty())
        .map(|value| PathBuf::from(std::ffi::OsStr::from_bytes(value)))
}

fn validate_absolute_normal_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        bail!("path must be absolute and normalized: {}", path.display());
    }
    Ok(())
}

fn owned_executable_regular_file(path: &Path, uid: u32) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()
            && metadata.uid() == uid
            && metadata.permissions().mode() & 0o111 != 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn owned_cargo_executable(path: &Path, rustup: &Path, uid: u32) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_file() {
        return Ok(metadata.uid() == uid && metadata.permissions().mode() & 0o111 != 0);
    }
    if !metadata.file_type().is_symlink() || metadata.uid() != uid {
        return Ok(false);
    }
    Ok(fs::read_link(path)? == Path::new("rustup")
        && path.parent() == rustup.parent()
        && rustup.file_name().is_some_and(|name| name == "rustup")
        && owned_executable_regular_file(rustup, uid)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_account_round_trips_through_nss() {
        let uid = rustix::process::getuid().as_raw();
        let by_uid = UnixAccount::by_uid(uid).unwrap();
        assert_eq!(UnixAccount::by_name(&by_uid.name).unwrap(), Some(by_uid));
    }

    #[test]
    fn missing_nss_accounts_and_groups_are_not_operational_errors() {
        assert!(
            UnixAccount::by_name("capulus-test-account-does-not-exist")
                .unwrap()
                .is_none()
        );
        assert!(
            group_gid("capulus-test-group-does-not-exist")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn process_environment_parser_is_binary_safe() {
        let environment = b"HOME=/home/example\0CARGO_HOME=/srv/cargo space\0EMPTY=\0";
        assert_eq!(
            environment_path(environment, b"CARGO_HOME"),
            Some(PathBuf::from("/srv/cargo space"))
        );
        assert_eq!(environment_path(environment, b"EMPTY"), None);
    }

    #[test]
    fn normalized_path_validation_rejects_traversal() {
        assert!(validate_absolute_normal_path(Path::new("/home/user/.cargo")).is_ok());
        assert!(validate_absolute_normal_path(Path::new("/home/user/../root")).is_err());
        assert!(validate_absolute_normal_path(Path::new("relative")).is_err());
    }

    #[test]
    fn standard_rustup_cargo_proxy_is_validated_without_following_arbitrary_links() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let rustup = directory.path().join("rustup");
        fs::write(&rustup, b"proxy").unwrap();
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o700)).unwrap();
        let cargo = directory.path().join("cargo");
        symlink("rustup", &cargo).unwrap();
        let uid = rustix::process::geteuid().as_raw();

        assert!(owned_cargo_executable(&cargo, &rustup, uid).unwrap());
        fs::remove_file(&cargo).unwrap();
        symlink("/bin/sh", &cargo).unwrap();
        assert!(!owned_cargo_executable(&cargo, &rustup, uid).unwrap());
    }

    #[test]
    fn account_command_uses_one_exact_setpriv_boundary() {
        let account = UnixAccount::by_uid(rustix::process::getuid().as_raw()).unwrap();
        let command = account.command("/bin/true", SupplementaryGroups::Initialize);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), OsStr::new("/usr/bin/setpriv"));
        assert!(arguments.contains(&format!("--reuid={}", account.uid)));
        assert!(arguments.contains(&format!("--regid={}", account.gid)));
        assert!(arguments.contains(&"--init-groups".to_string()));
        assert!(!arguments.contains(&"--reset-env".to_string()));
        assert_eq!(command.get_current_dir(), Some(account.home.as_path()));
        assert_eq!(
            command
                .get_envs()
                .find_map(|(name, value)| (name == "HOME").then_some(value))
                .flatten(),
            Some(account.home.as_os_str())
        );
    }

    #[test]
    fn build_boundaries_accept_only_current_prior_and_interrupted_states() {
        let prior_owner = DirectoryOwner { uid: 997, gid: 973 };
        assert_eq!(
            classify_test_boundary(0, 0, 0o711, prior_owner).unwrap(),
            BoundaryState::Current
        );
        assert_eq!(
            classify_test_boundary(prior_owner.uid, prior_owner.gid, 0o700, prior_owner).unwrap(),
            BoundaryState::PriorLayout
        );
        assert_eq!(
            classify_test_boundary(0, 0, 0o700, prior_owner).unwrap(),
            BoundaryState::OwnershipSecured
        );
        assert_eq!(
            PrivateDirectoryState::classify(
                DirectoryIdentity {
                    is_directory: true,
                    uid: prior_owner.uid,
                    gid: prior_owner.gid,
                    mode: 0o700,
                },
                prior_owner,
                DirectoryOwner::ROOT,
            )
            .unwrap(),
            PrivateDirectoryState::Current
        );
        assert_eq!(
            PrivateDirectoryState::classify(
                DirectoryIdentity {
                    is_directory: true,
                    uid: 0,
                    gid: 0,
                    mode: 0o700,
                },
                prior_owner,
                DirectoryOwner::ROOT,
            )
            .unwrap(),
            PrivateDirectoryState::CreationInterrupted
        );
        for mode in [0o000, 0o100, 0o300, 0o500, 0o600] {
            assert!(
                DirectoryIdentity {
                    is_directory: true,
                    uid: 0,
                    gid: 0,
                    mode,
                }
                .is_restricted_creation(DirectoryOwner::ROOT)
            );
        }
        for mode in [0o700, 0o701, 0o711, 0o1700] {
            assert!(
                !DirectoryIdentity {
                    is_directory: true,
                    uid: 0,
                    gid: 0,
                    mode,
                }
                .is_restricted_creation(DirectoryOwner::ROOT)
            );
        }
        for (is_directory, uid, gid, mode) in [
            (false, 0, 0, 0o711),
            (true, 0, 0, 0o755),
            (true, 0, 0, 0o701),
            (true, prior_owner.uid, prior_owner.gid, 0o711),
            (true, 1234, prior_owner.gid, 0o700),
        ] {
            assert!(
                BoundaryState::classify(
                    DirectoryIdentity {
                        is_directory,
                        uid,
                        gid,
                        mode,
                    },
                    prior_owner,
                    DirectoryOwner::ROOT,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn build_paths_require_one_canonical_home() {
        let temporary = tempfile::tempdir().unwrap();
        let mut build = test_build(temporary.path().join("capulus-build")).build;
        assert_eq!(
            build.home().unwrap(),
            temporary.path().join("capulus-build")
        );
        build.jobs_home = temporary.path().join("other/jobs");
        assert!(build.home().is_err());
    }

    #[test]
    fn prior_layout_is_migrated_once_without_replacing_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("capulus-build"));
        create_layout(&build, 0o700, 0o700);
        fs::write(build.cache_home.join("preserved"), b"cache").unwrap();
        fs::write(build.jobs_home.join("preserved"), b"job").unwrap();
        let inodes = layout_inodes(&build);

        build.ensure_layout().unwrap();
        assert_current_layout(&build);
        assert_eq!(
            fs::read(build.cache_home.join("preserved")).unwrap(),
            b"cache"
        );
        assert_eq!(fs::read(build.jobs_home.join("preserved")).unwrap(), b"job");
        assert_eq!(layout_inodes(&build), inodes);

        build.ensure_layout().unwrap();
        assert_current_layout(&build);
        assert_eq!(layout_inodes(&build), inodes);
    }

    #[test]
    fn migration_resumes_after_each_jobs_boundary_transition() {
        let temporary = tempfile::tempdir().unwrap();
        for (name, jobs_mode) in [("jobs-ownership-secured", 0o700), ("jobs-current", 0o711)] {
            let build = test_build(temporary.path().join(name));
            create_layout(&build, 0o700, jobs_mode);
            set_test_owner(&build.home, build.boundary_owner);
            set_test_owner(&build.jobs_home, build.boundary_owner);

            build.ensure_layout().unwrap();
            assert_current_layout(&build);
        }
    }

    #[test]
    fn current_layout_completes_missing_known_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("capulus-build"));
        create_test_directory(&build.home, 0o711);
        set_test_owner(&build.home, build.boundary_owner);
        for path in build.private_directories().into_iter().take(2) {
            create_test_directory(path, 0o700);
        }

        build.ensure_layout().unwrap();
        assert_current_layout(&build);

        let build = test_build(temporary.path().join("jobs-creation-interrupted"));
        create_layout(&build, 0o711, 0o500);
        set_test_owner(&build.jobs_home, build.boundary_owner);
        build.ensure_layout().unwrap();
        assert_current_layout(&build);
    }

    #[test]
    fn private_directory_creation_resumes_from_the_restricted_boundary_owner_state() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("capulus-build"));
        create_test_directory(&build.home, 0o700);
        set_test_owner(&build.home, build.boundary_owner);
        create_test_directory(&build.cache_home, 0o500);
        set_test_owner(&build.cache_home, build.boundary_owner);
        let inode = fs::symlink_metadata(&build.cache_home).unwrap().ino();

        build.ensure_private_directory(&build.cache_home).unwrap();

        assert_owned_mode(&build.cache_home, DirectoryOwner::of(&build.account), 0o700);
        assert_eq!(
            fs::symlink_metadata(&build.cache_home).unwrap().ino(),
            inode
        );
    }

    #[test]
    fn interrupted_private_directory_creation_must_still_be_empty() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("capulus-build"));
        create_test_directory(&build.home, 0o700);
        set_test_owner(&build.home, build.boundary_owner);
        create_test_directory(&build.cache_home, 0o700);
        set_test_owner(&build.cache_home, build.boundary_owner);
        fs::write(build.cache_home.join("unexpected"), b"data").unwrap();
        fs::set_permissions(&build.cache_home, fs::Permissions::from_mode(0o500)).unwrap();

        assert!(build.ensure_private_directory(&build.cache_home).is_err());
    }

    #[test]
    fn restricted_creation_and_interrupted_retry_are_umask_independent() {
        const CHILD: &str = "CAPULUS_RESTRICTIVE_UMASK_TEST";
        if std::env::var_os(CHILD).is_some() {
            let temporary = tempfile::tempdir().unwrap();
            // SAFETY: the filtered child test runs alone and exits without returning to callers.
            unsafe { libc::umask(0o777) };
            let build = test_build(temporary.path().join("capulus-build"));
            build.ensure_layout().unwrap();
            assert_current_layout(&build);

            // SAFETY: this remains the same isolated child test.
            unsafe { libc::umask(0o200) };
            let interrupted = temporary.path().join("interrupted");
            let owner = DirectoryOwner::of(&build.account);
            assert!(
                open_or_create_restricted_directory_with(&interrupted, owner, || {
                    bail!("injected post-mkdir failure")
                })
                .is_err()
            );
            assert_mode(&interrupted, 0o500);
            let (directory, initializing) =
                open_or_create_restricted_directory(&interrupted, owner).unwrap();
            assert!(initializing);
            assert!(
                DirectoryIdentity::from_metadata(&directory.metadata().unwrap())
                    .is_owned_directory(owner, 0o700)
            );
            return;
        }

        let status = Command::new(std::env::current_exe().unwrap())
            .arg("restricted_creation_and_interrupted_retry_are_umask_independent")
            .arg("--test-threads=1")
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn invalid_layouts_are_rejected_before_the_final_boundary_transition() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("invalid-home-mode"));
        create_layout(&build, 0o755, 0o700);
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o755);

        let build = test_build(temporary.path().join("nonempty-restricted-home"));
        create_test_directory(&build.home, 0o700);
        fs::write(build.home.join("unexpected"), b"data").unwrap();
        set_test_owner(&build.home, build.boundary_owner);
        fs::set_permissions(&build.home, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o500);

        let build = test_build(temporary.path().join("unexpected-entry"));
        create_layout(&build, 0o700, 0o700);
        fs::write(build.home.join("unexpected"), b"unmanaged").unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o700);

        let build = test_build(temporary.path().join("invalid-private"));
        create_layout(&build, 0o700, 0o700);
        fs::set_permissions(&build.cache_home, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o700);

        let build = test_build(temporary.path().join("nonempty-restricted-private"));
        create_layout(&build, 0o700, 0o700);
        fs::write(build.cache_home.join("unexpected"), b"data").unwrap();
        set_test_owner(&build.cache_home, build.boundary_owner);
        fs::set_permissions(&build.cache_home, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.cache_home, 0o500);

        let build = test_build(temporary.path().join("private-file"));
        create_layout(&build, 0o700, 0o700);
        fs::remove_dir(&build.cache_home).unwrap();
        fs::write(&build.cache_home, b"not a directory").unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o700);

        let build = test_build(temporary.path().join("private-symlink"));
        create_layout(&build, 0o700, 0o700);
        fs::remove_dir(&build.cache_home).unwrap();
        std::os::unix::fs::symlink(&build.rustup_home, &build.cache_home).unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o700);

        let build = test_build(temporary.path().join("prior-jobs-in-current-home"));
        create_layout(&build, 0o711, 0o700);
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.jobs_home, 0o700);

        let build = test_build(temporary.path().join("nonempty-current-jobs-creation"));
        create_layout(&build, 0o711, 0o700);
        set_test_owner(&build.jobs_home, build.boundary_owner);
        fs::write(build.jobs_home.join("unexpected"), b"data").unwrap();
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.jobs_home, 0o700);

        let build = test_build(temporary.path().join("invalid-interrupted-jobs"));
        create_layout(&build, 0o700, 0o755);
        assert!(build.ensure_layout().is_err());
        assert_mode(&build.home, 0o700);
        assert_mode(&build.jobs_home, 0o755);
    }

    #[test]
    fn target_reclamation_preserves_caches_and_recreates_an_exact_empty_target() {
        let temporary = tempfile::tempdir().unwrap();
        let build = test_build(temporary.path().join("capulus-build"));
        create_layout(&build, 0o711, 0o711);
        fs::create_dir(build.target_home.join("debug")).unwrap();
        fs::write(build.target_home.join("debug/artifact"), b"large target").unwrap();
        fs::write(build.cache_home.join("registry-index"), b"preserve cache").unwrap();
        let old_target = open_real_directory(&build.target_home).unwrap();
        let old_inode = old_target.metadata().unwrap().ino();

        build
            .build
            .reclaim_target_home_with(build.boundary_owner)
            .unwrap();

        assert_owned_mode(
            &build.target_home,
            DirectoryOwner::of(&build.account),
            0o700,
        );
        assert_ne!(
            fs::symlink_metadata(&build.target_home).unwrap().ino(),
            old_inode
        );
        assert!(fs::read_dir(&build.target_home).unwrap().next().is_none());
        assert_eq!(
            fs::read(build.cache_home.join("registry-index")).unwrap(),
            b"preserve cache"
        );

        fs::remove_dir(&build.target_home).unwrap();
        open_real_directory(&build.home)
            .unwrap()
            .sync_all()
            .unwrap();
        build.ensure_layout().unwrap();
        assert_owned_mode(
            &build.target_home,
            DirectoryOwner::of(&build.account),
            0o700,
        );
    }

    fn classify_test_boundary(
        uid: u32,
        gid: u32,
        mode: u32,
        prior_owner: DirectoryOwner,
    ) -> Result<BoundaryState> {
        BoundaryState::classify(
            DirectoryIdentity {
                is_directory: true,
                uid,
                gid,
                mode,
            },
            prior_owner,
            DirectoryOwner::ROOT,
        )
    }

    struct TestBuild {
        build: BuildAccount,
        home: PathBuf,
        boundary_owner: DirectoryOwner,
    }

    impl std::ops::Deref for TestBuild {
        type Target = BuildAccount;

        fn deref(&self) -> &Self::Target {
            &self.build
        }
    }

    impl TestBuild {
        fn ensure_layout(&self) -> Result<()> {
            self.build.ensure_layout_with(self.boundary_owner)
        }

        fn ensure_private_directory(&self, path: &Path) -> Result<()> {
            self.build
                .ensure_private_directory(path, self.boundary_owner)
        }
    }

    fn test_build(home: PathBuf) -> TestBuild {
        let account = UnixAccount::by_uid(rustix::process::geteuid().as_raw()).unwrap();
        let boundary_owner = DirectoryOwner::of(&account);
        let build = BuildAccount {
            account,
            cargo_tools_home: home.join("cargo-tools"),
            rustup_home: home.join("rustup"),
            cache_home: home.join("cache"),
            target_home: home.join("target"),
            jobs_home: home.join("jobs"),
        };
        TestBuild {
            build,
            home,
            boundary_owner,
        }
    }

    fn create_layout(build: &TestBuild, home_mode: u32, jobs_mode: u32) {
        create_test_directory(&build.home, home_mode);
        if home_mode == 0o711 {
            set_test_owner(&build.home, build.boundary_owner);
        }
        for path in build.private_directories() {
            create_test_directory(path, 0o700);
        }
        create_test_directory(&build.jobs_home, jobs_mode);
        if jobs_mode == 0o711 {
            set_test_owner(&build.jobs_home, build.boundary_owner);
        }
    }

    fn create_test_directory(path: &Path, mode: u32) {
        let mut builder = DirBuilder::new();
        builder.mode(mode).create(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn set_test_owner(path: &Path, owner: DirectoryOwner) {
        set_directory_owner_and_sync(&open_real_directory(path).unwrap(), owner, "test ownership")
            .unwrap();
    }

    fn assert_current_layout(build: &TestBuild) {
        assert_owned_mode(&build.home, build.boundary_owner, 0o711);
        for path in build.private_directories() {
            assert_owned_mode(path, DirectoryOwner::of(&build.account), 0o700);
        }
        assert_owned_mode(&build.jobs_home, build.boundary_owner, 0o711);
    }

    fn assert_owned_mode(path: &Path, owner: DirectoryOwner, mode: u32) {
        let identity = DirectoryIdentity::from_metadata(&fs::symlink_metadata(path).unwrap());
        assert!(identity.is_owned_directory(owner, mode));
    }

    fn assert_mode(path: &Path, mode: u32) {
        assert_eq!(
            fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }

    fn layout_inodes(build: &TestBuild) -> Vec<u64> {
        std::iter::once(build.home.as_path())
            .chain(build.private_directories())
            .chain(std::iter::once(build.jobs_home.as_path()))
            .map(|path| fs::symlink_metadata(path).unwrap().ino())
            .collect()
    }
}
