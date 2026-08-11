use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};

use super::account::{
    SupplementaryGroups, UnixAccount, ensure_owned_directory, ensure_root_directory, require_root,
};
use super::build::run_with_deadline;
use super::product::is_valid_identifier;
use super::{CargoRegistry, JobId, ManagedProduct, ResolvedRelease, UserInstallContext};

const USER_INSTALL_ROOT: &str = "/run/capulus/user-installs";

pub fn reinstall_user_cli(
    product: &ManagedProduct,
    job: JobId,
    release: &ResolvedRelease,
    context: &UserInstallContext,
) -> Result<()> {
    require_root()?;
    let account = context.revalidate(&product.user_binary().cargo_name)?;
    let root = Path::new(USER_INSTALL_ROOT).join(format!("{}-{job}", product.name()));
    let ephemeral = EphemeralCargoHome::create(&root, &account, &release.registry)?;
    let cargo_home = ephemeral.path();
    let mut command = account.command(&context.cargo, SupplementaryGroups::Initialize);
    command
        .env("CARGO_HOME", cargo_home)
        .env("RUSTUP_HOME", &context.rustup_home)
        .env("CARGO_TARGET_DIR", cargo_home.join("target"))
        .env("PATH", user_path(&context.cargo_home))
        .args(["install", "--locked", "--force", "--root"])
        .arg(&context.cargo_home)
        .args(["--version", &release.version.to_string()])
        .arg(product.package())
        .args(["--bin", &product.user_binary().cargo_name]);
    if let Some(registry) = release.registry.cargo_registry_name() {
        command.args(["--registry", registry]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    run_with_deadline(
        &mut command,
        product.build_timeout(),
        "reinstall the requesting user's Cargo CLI",
    )?;
    let account = context.revalidate(&product.user_binary().cargo_name)?;
    verify_installed_version(release, context, cargo_home, &account)?;
    Ok(())
}

fn verify_installed_version(
    release: &ResolvedRelease,
    context: &UserInstallContext,
    ephemeral_cargo_home: &Path,
    account: &UnixAccount,
) -> Result<()> {
    let output = ephemeral_cargo_home.join("installed-version.txt");
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

struct EphemeralCargoHome {
    path: PathBuf,
}

impl EphemeralCargoHome {
    fn create(path: &Path, account: &UnixAccount, registry: &CargoRegistry) -> Result<Self> {
        ensure_root_directory(Path::new(USER_INSTALL_ROOT), 0o711)?;
        ensure_owned_directory(path, account, 0o700)?;
        let ephemeral = Self {
            path: path.to_path_buf(),
        };
        let ca_path = path.join("registry-ca.pem");
        let configuration = registry.configuration(&ca_path)?;
        if let Some(credentials) = configuration.credentials {
            account.write_file(
                &path.join("credentials.toml"),
                credentials.as_bytes(),
                0o600,
            )?;
            account.write_file(
                &ca_path,
                configuration
                    .ca_pem
                    .expect("private Cargo credentials always have a CA bundle")
                    .as_bytes(),
                0o600,
            )?;
        }
        account.write_file(
            &path.join("config.toml"),
            configuration.config.as_bytes(),
            0o600,
        )?;
        Ok(ephemeral)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EphemeralCargoHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
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

pub(super) fn remove_orphaned_user_installations() -> Result<()> {
    let root = Path::new(USER_INSTALL_ROOT);
    ensure_root_directory(root, 0o711)?;
    for entry in fs::read_dir(root).context("failed to inspect ephemeral user installations")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("ephemeral user installation has a non-UTF-8 name"))?;
        let Some((product, job)) = name.rsplit_once('-') else {
            bail!("ephemeral user installation has an invalid name: {name:?}");
        };
        if !is_valid_identifier(product) || JobId::parse(job).is_err() {
            bail!("ephemeral user installation has an invalid name: {name:?}");
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.file_type().is_dir() || metadata.uid() == 0 {
            bail!("ephemeral user installation is not a user-owned real directory: {name:?}");
        }
        fs::remove_dir_all(entry.path())
            .with_context(|| format!("failed to remove orphaned user installation {name}"))?;
    }
    File::open(root)?.sync_all().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_path_prefixes_the_callers_cargo_bin() {
        assert_eq!(
            user_path(Path::new("/home/example/.cargo")),
            OsString::from("/home/example/.cargo/bin:/usr/local/bin:/usr/bin:/bin")
        );
    }
}
