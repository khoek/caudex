use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use rustix::fs::{Gid, Uid, chown};

use super::account::require_root;
use super::build::run_with_deadline;
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
    let ephemeral = EphemeralCargoHome::create(&root, account.uid, account.gid, &release.registry)?;
    let cargo_home = ephemeral.path();
    let mut command = Command::new("/usr/bin/setpriv");
    command
        .env_clear()
        .args([
            format!("--reuid={}", account.uid),
            format!("--regid={}", account.gid),
            "--init-groups".to_string(),
            "--reset-env".to_string(),
            "--".to_string(),
            "/usr/bin/env".to_string(),
        ])
        .arg(environment_assignment("HOME", &account.home))
        .arg(environment_assignment("USER", &account.name))
        .arg(environment_assignment("LOGNAME", &account.name))
        .arg(environment_assignment("CARGO_HOME", cargo_home))
        .arg(environment_assignment("RUSTUP_HOME", &context.rustup_home))
        .arg(environment_assignment(
            "CARGO_TARGET_DIR",
            cargo_home.join("target"),
        ))
        .arg(environment_assignment(
            "PATH",
            user_path(&context.cargo_home),
        ))
        .arg("LANG=C.UTF-8")
        .arg(&context.cargo)
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
    context.revalidate(&product.user_binary().cargo_name)?;
    verify_installed_version(release, context, cargo_home)?;
    Ok(())
}

fn verify_installed_version(
    release: &ResolvedRelease,
    context: &UserInstallContext,
    ephemeral_cargo_home: &Path,
) -> Result<()> {
    let output = ephemeral_cargo_home.join("installed-version.txt");
    let output_file = create_user_file(&output, context.uid, context.gid, 0o600)?;
    let mut command = Command::new("/usr/bin/setpriv");
    command
        .env_clear()
        .args([
            format!("--reuid={}", context.uid),
            format!("--regid={}", context.gid),
            "--init-groups".to_string(),
            "--reset-env".to_string(),
            "--".to_string(),
        ])
        .arg(&context.installed_binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::inherit());
    run_with_deadline(
        &mut command,
        std::time::Duration::from_secs(30),
        "verify the reinstalled user CLI version",
    )?;
    let value = fs::read_to_string(output)?;
    if value.split_whitespace().last() != Some(release.version.to_string().as_str()) {
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
    fn create(path: &Path, uid: u32, gid: u32, registry: &CargoRegistry) -> Result<Self> {
        ensure_root_directory(Path::new(USER_INSTALL_ROOT), 0o711)?;
        if path.exists() {
            bail!(
                "ephemeral user Cargo home already exists: {}",
                path.display()
            );
        }
        fs::create_dir(path)?;
        let ephemeral = Self {
            path: path.to_path_buf(),
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
            .map_err(std::io::Error::from)?;
        let ca_path = path.join("registry-ca.pem");
        let configuration = registry.configuration(&ca_path)?;
        if let Some(credentials) = configuration.credentials {
            write_user_file(
                &path.join("credentials.toml"),
                credentials.as_bytes(),
                uid,
                gid,
                0o600,
            )?;
            write_user_file(
                &ca_path,
                configuration
                    .ca_pem
                    .expect("private Cargo credentials always have a CA bundle")
                    .as_bytes(),
                uid,
                gid,
                0o600,
            )?;
        }
        write_user_file(
            &path.join("config.toml"),
            configuration.config.as_bytes(),
            uid,
            gid,
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

fn write_user_file(path: &Path, bytes: &[u8], uid: u32, gid: u32, mode: u32) -> Result<()> {
    let mut file = create_user_file(path, uid, gid, mode)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn create_user_file(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<fs::File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))
        .map_err(std::io::Error::from)?;
    Ok(file)
}

fn environment_assignment(name: &str, value: impl AsRef<OsStr>) -> OsString {
    let value = value.as_ref();
    let mut assignment = Vec::with_capacity(name.len() + 1 + value.as_bytes().len());
    assignment.extend_from_slice(name.as_bytes());
    assignment.push(b'=');
    assignment.extend_from_slice(value.as_bytes());
    OsString::from_vec(assignment)
}

fn user_path(cargo_home: &Path) -> OsString {
    let mut value = cargo_home.join("bin").into_os_string();
    value.push(":/usr/local/bin:/usr/bin:/bin");
    value
}

fn ensure_root_directory(path: &Path, mode: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_dir() || metadata.uid() != 0 || metadata.gid() != 0 =>
        {
            bail!(
                "Capulus runtime path is not a root-owned directory: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_assignment_preserves_non_utf8_paths() {
        assert_eq!(
            environment_assignment("HOME", OsString::from_vec(vec![b'/', 0xff])).as_bytes(),
            &[b'H', b'O', b'M', b'E', b'=', b'/', 0xff]
        );
    }
}
