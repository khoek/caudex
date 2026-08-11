use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use rustix::fs::{Gid, Uid, chown};
use serde::{Deserialize, Serialize};

use super::PeerCredentials;

pub const BUILD_USER: &str = "capulus-build";
pub const BUILD_GROUP: &str = "capulus-build";
pub const BUILD_HOME: &str = "/var/lib/capulus-build";
pub const BUILD_CARGO_TOOLS_HOME: &str = "/var/lib/capulus-build/cargo-tools";
pub const BUILD_RUSTUP_HOME: &str = "/var/lib/capulus-build/rustup";
pub const BUILD_CACHE_HOME: &str = "/var/lib/capulus-build/cache";
pub const BUILD_TARGET_HOME: &str = "/var/lib/capulus-build/target";
pub const BUILD_JOBS_HOME: &str = "/var/lib/capulus-build/jobs";

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
                    "--create-home",
                    "--shell",
                    "/usr/sbin/nologin",
                    BUILD_USER,
                ]),
                "create shared Capulus build account",
            )?;
        }
        let account = UnixAccount::by_name(BUILD_USER)?
            .ok_or_else(|| anyhow!("shared Capulus build account was not created"))?;
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
        ensure_owned_directory(Path::new(BUILD_HOME), &build.account, 0o700)?;
        for path in [
            &build.cargo_tools_home,
            &build.rustup_home,
            &build.cache_home,
            &build.target_home,
            &build.jobs_home,
        ] {
            ensure_owned_directory(path, &build.account, 0o700)?;
        }
        Ok(build)
    }

    pub fn cargo(&self) -> PathBuf {
        self.cargo_tools_home.join("bin/cargo")
    }

    pub fn rustup(&self) -> PathBuf {
        self.cargo_tools_home.join("bin/rustup")
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
            fs::create_dir_all(path)
                .with_context(|| format!("failed to create {}", path.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    chown(
        path,
        Some(Uid::from_raw(account.uid)),
        Some(Gid::from_raw(account.gid)),
    )
    .map_err(std::io::Error::from)
    .with_context(|| format!("failed to chown {}", path.display()))
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
    let contents =
        fs::read_to_string("/etc/login.defs").context("failed to read /etc/login.defs")?;
    for line in contents.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let mut fields = line.split_whitespace();
        if fields.next() == Some("UID_MIN") {
            return fields
                .next()
                .ok_or_else(|| anyhow!("UID_MIN has no value in /etc/login.defs"))?
                .parse()
                .context("UID_MIN is invalid in /etc/login.defs");
        }
    }
    bail!("/etc/login.defs does not define UID_MIN")
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
}
