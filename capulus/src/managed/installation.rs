use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::account::require_root;
use super::build::run_with_deadline;
use super::{
    BuildArtifacts, JobId, ManagedFile, ManagedProduct, RepairOutcome, SystemdManager, UnixAccount,
};

const INSTALLATION_STATE_ROOT: &str = "/var/lib/capulus/installations";
const INSTALLATION_LOCK_ROOT: &str = "/run/capulus/locks";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SystemInstallation {
    product: ManagedProduct,
    journal_path: PathBuf,
    journal: InstallationJournal,
    files_committed: bool,
    _lock: crate::InvocationLock,
}

impl SystemInstallation {
    pub async fn inspect_and_repair(product: &ManagedProduct) -> Result<RepairOutcome> {
        require_root()?;
        let _lock = acquire_installation_lock(product)?;
        ensure_installation_state_directory(product)?;
        let installation_journal = Path::new(INSTALLATION_STATE_ROOT)
            .join(product.name())
            .join("active.json");
        if installation_journal.exists() {
            Self::recover(product, &installation_journal).await?;
        }
        recover_uninstallation(product).await?;
        let mut changed = false;
        for managed in product.installation_manifest().files {
            match managed {
                ManagedFile::Binary {
                    destination, mode, ..
                } => {
                    let metadata = fs::symlink_metadata(&destination).with_context(|| {
                        format!("managed binary is missing: {}", destination.display())
                    })?;
                    validate_installed_file(&destination, &metadata)?;
                    if metadata.mode() & 0o777 != mode {
                        fs::set_permissions(&destination, fs::Permissions::from_mode(mode))?;
                        changed = true;
                    }
                }
                ManagedFile::Text {
                    destination,
                    contents,
                    mode,
                } => {
                    validate_root_directory(
                        destination
                            .parent()
                            .expect("validated unit path has a parent"),
                    )?;
                    let matches = match fs::symlink_metadata(&destination) {
                        Ok(metadata) => {
                            validate_installed_file(&destination, &metadata)?;
                            metadata.mode() & 0o777 == mode
                                && fs::read(&destination)? == contents.as_bytes()
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                        Err(error) => return Err(error.into()),
                    };
                    if !matches {
                        crate::store::atomic_write(
                            &destination,
                            contents.as_bytes(),
                            Some(mode),
                            None,
                        )?;
                        changed = true;
                    }
                }
            }
        }
        SystemdManager.refresh_installation(product).await?;
        Ok(RepairOutcome {
            changed,
            detail: if changed {
                "restored managed files and refreshed all managed units".to_string()
            } else {
                "managed files are intact and all managed units are enabled".to_string()
            },
        })
    }

    pub async fn prepare(
        product: &ManagedProduct,
        job: JobId,
        artifacts: &BuildArtifacts,
        build_account: &UnixAccount,
    ) -> Result<Self> {
        require_root()?;
        let lock = acquire_installation_lock(product)?;
        let state_directory = ensure_installation_state_directory(product)?;
        let journal_path = state_directory.join("active.json");
        if journal_path.exists() {
            Self::recover(product, &journal_path).await?;
        }
        recover_uninstallation(product).await?;
        artifacts.validate(product, build_account)?;
        let previously_enabled = SystemdManager.enabled_units(product).await?;
        let target_files = artifacts
            .manifest
            .files
            .iter()
            .map(|file| (managed_destination(file).to_path_buf(), file))
            .collect::<BTreeMap<_, _>>();
        let mut records = artifacts
            .manifest
            .files
            .iter()
            .map(|file| {
                plan_record(
                    product,
                    job,
                    artifacts,
                    build_account,
                    managed_destination(file),
                    Some(file),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        for destination in product
            .installation_manifest()
            .files
            .iter()
            .map(managed_destination)
            .filter(|destination| !target_files.contains_key(*destination))
        {
            records.push(plan_record(
                product,
                job,
                artifacts,
                build_account,
                destination,
                None,
            )?);
        }
        let previous_service_file = records.iter().any(|record| {
            record.destination == Path::new("/etc/systemd/system").join(product.service_name())
                && record.original_digest.is_some()
        });
        let journal = InstallationJournal {
            product: product.name().to_string(),
            job,
            previously_enabled,
            target_enable_units: artifacts.manifest.enable_units.clone(),
            previous_service_file,
            committed_records: 0,
            accepted: false,
            records,
        };
        write_journal(&journal_path, &journal)?;
        let installation = Self {
            product: product.clone(),
            journal_path,
            journal,
            files_committed: false,
            _lock: lock,
        };
        for record in &installation.journal.records {
            if let Err(error) = stage_record(
                record,
                artifacts,
                build_account,
                target_files.get(&record.destination).copied(),
            ) {
                installation.cleanup_staging()?;
                remove_private_file(&installation.journal_path)?;
                return Err(error);
            }
        }
        Ok(installation)
    }

    pub fn commit_files(&mut self) -> Result<()> {
        for index in 0..self.journal.records.len() {
            let record = &self.journal.records[index];
            validate_destination_before_commit(record)?;
            if record.original_digest.is_some() {
                fs::rename(&record.destination, &record.backup).with_context(|| {
                    format!(
                        "failed to preserve installed file {}",
                        record.destination.display()
                    )
                })?;
            }
            if record.new_digest.is_some() {
                fs::rename(&record.staged, &record.destination).with_context(|| {
                    format!(
                        "failed to install staged file {}",
                        record.destination.display()
                    )
                })?;
            }
            sync_directory(
                record
                    .destination
                    .parent()
                    .expect("validated destinations have parents"),
            )?;
            self.journal.committed_records = index + 1;
            write_journal(&self.journal_path, &self.journal)?;
        }
        self.files_committed = true;
        self.verify_committed_units()
    }

    pub async fn activate(&self) -> Result<()> {
        if !self.files_committed {
            bail!("cannot activate an uncommitted Capulus installation");
        }
        SystemdManager
            .activate_installation(
                &self.product,
                &self.journal.target_enable_units,
                &self.journal.previously_enabled,
                self.journal.previous_service_file,
                self.journal.previous_application_socket_file(),
            )
            .await
            .map_err(Into::into)
    }

    pub async fn rollback(&mut self) -> Result<()> {
        if self.journal.accepted {
            bail!("cannot roll back an accepted Capulus installation");
        }
        let filesystem_result = rollback_files(&self.journal);
        let systemd_result = SystemdManager
            .restore_installation(
                &self.product,
                &self.journal.target_enable_units,
                &self.journal.previously_enabled,
                self.journal.previous_service_file,
                self.journal.previous_application_socket_file(),
            )
            .await;
        match (filesystem_result, systemd_result) {
            (Ok(()), Ok(())) => {
                self.files_committed = false;
                self.cleanup_staging()?;
                remove_private_file(&self.journal_path)
            }
            (Err(filesystem), Ok(())) => Err(filesystem),
            (Ok(()), Err(systemd)) => Err(systemd.into()),
            (Err(filesystem), Err(systemd)) => Err(anyhow!(
                "filesystem rollback failed: {filesystem:#}; systemd restoration failed: {systemd}"
            )),
        }
    }

    pub fn finalize(&mut self) -> Result<()> {
        if !self.files_committed {
            bail!("cannot finalize an uncommitted Capulus installation");
        }
        self.journal.accepted = true;
        write_journal(&self.journal_path, &self.journal)?;
        self.cleanup_staging()?;
        remove_private_file(&self.journal_path)
    }

    pub fn files_committed(&self) -> bool {
        self.files_committed
    }

    pub fn acceptance_committed(&self) -> bool {
        self.journal.accepted
    }

    async fn recover(product: &ManagedProduct, journal_path: &Path) -> Result<()> {
        validate_private_file(journal_path)?;
        let journal: InstallationJournal = serde_json::from_slice(&fs::read(journal_path)?)
            .context("failed to decode interrupted Capulus installation journal")?;
        journal.validate(product.name())?;
        if journal.accepted {
            validate_committed_files(&journal)?;
            SystemdManager.refresh_installation(product).await?;
        } else {
            rollback_files(&journal)?;
            SystemdManager
                .restore_installation(
                    product,
                    &journal.target_enable_units,
                    &journal.previously_enabled,
                    journal.previous_service_file,
                    journal.previous_application_socket_file(),
                )
                .await?;
        }
        cleanup_staging(&journal)?;
        remove_private_file(journal_path)
    }

    fn verify_committed_units(&self) -> Result<()> {
        let units = committed_unit_paths(&self.journal);
        let mut command = Command::new("/usr/bin/systemd-analyze");
        command
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LANG", "C.UTF-8")
            .arg("verify")
            .args(&units)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        run_with_deadline(
            &mut command,
            VERIFY_TIMEOUT,
            "verify committed systemd units",
        )?;
        Ok(())
    }

    fn cleanup_staging(&self) -> Result<()> {
        cleanup_staging(&self.journal)
    }
}

fn committed_unit_paths(journal: &InstallationJournal) -> Vec<&Path> {
    journal
        .records
        .iter()
        .filter(|record| record.destination.parent() == Some(Path::new("/etc/systemd/system")))
        .filter(|record| record.new_digest.is_some())
        .map(|record| record.destination.as_path())
        .collect()
}

pub struct SystemUninstallation {
    product: ManagedProduct,
    journal_path: PathBuf,
    journal: UninstallationJournal,
    deactivated: bool,
    _lock: crate::InvocationLock,
}

impl SystemUninstallation {
    pub async fn prepare(product: &ManagedProduct, job: JobId) -> Result<Self> {
        require_root()?;
        let lock = acquire_installation_lock(product)?;
        let state_directory = ensure_installation_state_directory(product)?;
        let installation_journal = state_directory.join("active.json");
        if installation_journal.exists() {
            SystemInstallation::recover(product, &installation_journal).await?;
        }
        recover_uninstallation(product).await?;
        let mut records = product
            .installation_manifest()
            .files
            .into_iter()
            .map(|managed| plan_uninstallation_record(product, job, managed))
            .collect::<Result<Vec<_>>>()?;
        records.sort_by_key(|record| record.destination == product.agent_executable());
        let journal = UninstallationJournal {
            product: product.name().to_string(),
            job,
            previously_enabled: SystemdManager.enabled_units(product).await?,
            committed_records: 0,
            removal_committed: false,
            records,
        };
        let journal_path = state_directory.join("uninstall.json");
        write_uninstallation_journal(&journal_path, &journal)?;
        Ok(Self {
            product: product.clone(),
            journal_path,
            journal,
            deactivated: false,
            _lock: lock,
        })
    }

    pub async fn deactivate(&mut self) -> Result<()> {
        SystemdManager
            .deactivate_installation(&self.product)
            .await?;
        self.deactivated = true;
        Ok(())
    }

    pub fn remove_files(&mut self) -> Result<()> {
        if !self.deactivated {
            bail!("cannot remove managed files before deactivating their systemd units");
        }
        for index in 0..self.journal.records.len() {
            let record = &self.journal.records[index];
            let metadata = fs::symlink_metadata(&record.destination).with_context(|| {
                format!(
                    "managed uninstall destination disappeared: {}",
                    record.destination.display()
                )
            })?;
            validate_installed_file(&record.destination, &metadata)?;
            if file_digest(&record.destination)? != record.digest {
                bail!(
                    "managed file changed before uninstall: {}",
                    record.destination.display()
                );
            }
            ensure_root_directory(
                record
                    .backup
                    .parent()
                    .and_then(Path::parent)
                    .expect("validated uninstall backup has a root"),
                0o700,
            )?;
            ensure_root_directory(
                record
                    .backup
                    .parent()
                    .expect("validated uninstall backup has a parent"),
                0o700,
            )?;
            fs::rename(&record.destination, &record.backup).with_context(|| {
                format!(
                    "failed to preserve managed file during uninstall: {}",
                    record.destination.display()
                )
            })?;
            sync_directory(
                record
                    .destination
                    .parent()
                    .expect("validated destination has a parent"),
            )?;
            self.journal.committed_records = index + 1;
            write_uninstallation_journal(&self.journal_path, &self.journal)?;
        }
        Ok(())
    }

    pub async fn finalize(&mut self) -> Result<()> {
        if self.journal.committed_records != self.journal.records.len() {
            bail!("cannot finalize an incomplete managed uninstall");
        }
        SystemdManager.reload_removed_installation().await?;
        self.journal.removal_committed = true;
        write_uninstallation_journal(&self.journal_path, &self.journal)?;
        cleanup_uninstallation_staging(&self.journal)?;
        remove_private_file(&self.journal_path)
    }

    pub fn removal_committed(&self) -> bool {
        self.journal.removal_committed
    }

    pub async fn rollback(&mut self) -> Result<()> {
        let filesystem = restore_uninstallation_files(&self.journal);
        let systemd = SystemdManager
            .restore_installation(
                &self.product,
                &self.product.installation_manifest().enable_units,
                &self.journal.previously_enabled,
                true,
                self.product.application_socket_is_systemd_activated(),
            )
            .await;
        match (filesystem, systemd) {
            (Ok(()), Ok(())) => {
                cleanup_uninstallation_staging(&self.journal)?;
                remove_private_file(&self.journal_path)
            }
            (Err(filesystem), Ok(())) => Err(filesystem),
            (Ok(()), Err(systemd)) => Err(systemd.into()),
            (Err(filesystem), Err(systemd)) => Err(anyhow!(
                "uninstall filesystem rollback failed: {filesystem:#}; systemd restoration failed: {systemd}"
            )),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UninstallationJournal {
    product: String,
    job: JobId,
    previously_enabled: Vec<String>,
    committed_records: usize,
    removal_committed: bool,
    records: Vec<UninstallationRecord>,
}

impl UninstallationJournal {
    fn validate(&self, product: &ManagedProduct) -> Result<()> {
        if self.product != product.name()
            || self.records.is_empty()
            || self.committed_records > self.records.len()
            || (self.removal_committed && self.committed_records != self.records.len())
        {
            bail!("interrupted Capulus uninstall journal has the wrong identity");
        }
        let expected = product
            .installation_manifest()
            .files
            .into_iter()
            .map(|managed| match managed {
                ManagedFile::Binary { destination, .. } | ManagedFile::Text { destination, .. } => {
                    destination
                }
            })
            .collect::<BTreeSet<_>>();
        let stage_component = format!(".capulus-{}-uninstall-{}", product.name(), self.job);
        let mut destinations = BTreeSet::new();
        for record in &self.records {
            validate_normal_absolute(&record.destination)?;
            let parent = record
                .destination
                .parent()
                .expect("validated uninstall destination has a parent");
            let name = record
                .destination
                .file_name()
                .expect("validated uninstall destination has a filename");
            if record.backup != parent.join(&stage_component).join("old").join(name)
                || !valid_digest(&record.digest)
                || !destinations.insert(record.destination.clone())
            {
                bail!("interrupted Capulus uninstall journal contains an unsafe path");
            }
        }
        if destinations != expected {
            bail!("interrupted Capulus uninstall journal has the wrong managed files");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UninstallationRecord {
    destination: PathBuf,
    backup: PathBuf,
    digest: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallationJournal {
    product: String,
    job: JobId,
    previously_enabled: Vec<String>,
    target_enable_units: Vec<String>,
    previous_service_file: bool,
    committed_records: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    accepted: bool,
    records: Vec<InstallationRecord>,
}

impl InstallationJournal {
    fn previous_application_socket_file(&self) -> bool {
        self.records.iter().any(|record| {
            record.destination
                == Path::new("/etc/systemd/system").join(format!("{}-agent.socket", self.product))
                && record.original_digest.is_some()
        })
    }

    fn validate(&self, product: &str) -> Result<()> {
        if self.product != product
            || self.records.is_empty()
            || self.committed_records > self.records.len()
            || (self.accepted && self.committed_records != self.records.len())
            || self.target_enable_units.is_empty()
        {
            bail!("interrupted Capulus installation journal has the wrong identity");
        }
        let allowed_units = format!("{product}-");
        let stage_component = format!(".capulus-{product}-{}", self.job);
        let mut destinations = BTreeSet::new();
        for record in &self.records {
            validate_normal_absolute(&record.destination)?;
            let valid_destination =
                if record.destination.parent() == Some(Path::new("/usr/local/bin")) {
                    record
                        .destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == product || name.starts_with(&allowed_units))
                } else if record.destination.parent() == Some(Path::new("/etc/systemd/system")) {
                    record
                        .destination
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with(&allowed_units)
                                && (name.ends_with(".service") || name.ends_with(".socket"))
                        })
                } else {
                    false
                };
            let parent = record
                .destination
                .parent()
                .expect("validated destination has a parent");
            let name = record
                .destination
                .file_name()
                .expect("validated destination has a filename");
            if !valid_destination
                || record.staged != parent.join(&stage_component).join("new").join(name)
                || record.backup != parent.join(&stage_component).join("old").join(name)
                || record
                    .new_digest
                    .as_ref()
                    .is_some_and(|digest| !valid_digest(digest))
                || record
                    .original_digest
                    .as_ref()
                    .is_some_and(|digest| !valid_digest(digest))
                || !destinations.insert(record.destination.clone())
            {
                bail!("interrupted Capulus installation journal contains an unsafe path");
            }
        }
        let target_units = self.target_enable_units.iter().collect::<BTreeSet<_>>();
        if target_units.len() != self.target_enable_units.len()
            || target_units.iter().any(|unit| {
                !unit.starts_with(&allowed_units)
                    || !(unit.ends_with(".service") || unit.ends_with(".socket"))
                    || !self.records.iter().any(|record| {
                        record.destination == Path::new("/etc/systemd/system").join(unit)
                            && record.new_digest.is_some()
                    })
            })
        {
            bail!("interrupted Capulus installation journal has unsafe target units");
        }
        let previous_file_exists = |unit: &str| {
            let path = Path::new("/etc/systemd/system").join(unit);
            self.records
                .iter()
                .any(|record| record.destination == path && record.original_digest.is_some())
        };
        if self.previous_service_file != previous_file_exists(&format!("{product}-agent.service")) {
            bail!("interrupted Capulus installation journal has inconsistent prior units");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallationRecord {
    destination: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    original_digest: Option<String>,
    new_digest: Option<String>,
}

fn plan_record(
    product: &ManagedProduct,
    job: JobId,
    artifacts: &BuildArtifacts,
    build_account: &UnixAccount,
    destination: &Path,
    managed: Option<&ManagedFile>,
) -> Result<InstallationRecord> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("managed destination has no parent"))?;
    validate_root_directory(parent)?;
    let stage_root = parent.join(format!(".capulus-{}-{job}", product.name()));
    let filename = destination
        .file_name()
        .ok_or_else(|| anyhow!("managed destination has no filename"))?;
    let staged = stage_root.join("new").join(filename);
    let backup = stage_root.join("old").join(filename);
    let new_digest = managed
        .map(|managed| match managed {
            ManagedFile::Binary { source_name, .. } => validated_artifact_digest(
                &artifacts.binary_directory.join(source_name),
                build_account,
            ),
            ManagedFile::Text { contents, .. } => Ok(bytes_digest(contents.as_bytes())),
        })
        .transpose()?;
    let original_digest = match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            validate_installed_file(destination, &metadata)?;
            Some(file_digest(destination)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", destination.display()));
        }
    };
    Ok(InstallationRecord {
        destination: destination.to_path_buf(),
        staged,
        backup,
        original_digest,
        new_digest,
    })
}

fn stage_record(
    record: &InstallationRecord,
    artifacts: &BuildArtifacts,
    build_account: &UnixAccount,
    managed: Option<&ManagedFile>,
) -> Result<()> {
    let stage_root = record
        .staged
        .parent()
        .and_then(Path::parent)
        .expect("validated staged path has a root");
    ensure_root_directory(stage_root, 0o700)?;
    ensure_root_directory(
        record
            .staged
            .parent()
            .expect("validated staged path has a parent"),
        0o700,
    )?;
    ensure_root_directory(
        record
            .backup
            .parent()
            .expect("validated backup path has a parent"),
        0o700,
    )?;
    match managed {
        Some(ManagedFile::Binary {
            source_name, mode, ..
        }) => copy_build_artifact(
            &artifacts.binary_directory.join(source_name),
            &record.staged,
            build_account,
            *mode,
        )?,
        Some(ManagedFile::Text { contents, mode, .. }) => {
            write_root_file(&record.staged, contents.as_bytes(), *mode)?
        }
        None => return Ok(()),
    }
    if Some(file_digest(&record.staged)?) != record.new_digest {
        bail!("staged file changed while the installation was prepared");
    }
    sync_directory(
        record
            .staged
            .parent()
            .expect("validated staged path has a parent"),
    )
}

fn validated_artifact_digest(source: &Path, account: &UnixAccount) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(source)
        .with_context(|| format!("failed to open staged artifact {}", source.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != account.uid || metadata.gid() != account.gid {
        bail!(
            "staged artifact is not a build-account-owned regular file: {}",
            source.display()
        );
    }
    reader_digest(file)
}

fn copy_build_artifact(
    source: &Path,
    destination: &Path,
    account: &UnixAccount,
    mode: u32,
) -> Result<()> {
    let mut source_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(source)
        .with_context(|| format!("failed to open staged artifact {}", source.display()))?;
    let metadata = source_file.metadata()?;
    if !metadata.is_file() || metadata.uid() != account.uid || metadata.gid() != account.gid {
        bail!(
            "staged artifact is not a build-account-owned regular file: {}",
            source.display()
        );
    }
    let mut destination_file = create_root_file(destination, mode)?;
    std::io::copy(&mut source_file, &mut destination_file)?;
    destination_file.sync_all()?;
    Ok(())
}

fn write_root_file(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let mut file = create_root_file(path, mode)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn create_root_file(path: &Path, mode: u32) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create staged file {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(file)
}

fn validate_destination_before_commit(record: &InstallationRecord) -> Result<()> {
    match (
        &record.original_digest,
        fs::symlink_metadata(&record.destination),
    ) {
        (Some(expected), Ok(metadata)) => {
            validate_installed_file(&record.destination, &metadata)?;
            if file_digest(&record.destination)? != *expected {
                bail!(
                    "installed file changed while redeploy was staging: {}",
                    record.destination.display()
                );
            }
        }
        (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        (Some(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "installed file disappeared while redeploy was staging: {}",
                record.destination.display()
            )
        }
        (None, Ok(_)) => bail!(
            "managed destination appeared while redeploy was staging: {}",
            record.destination.display()
        ),
        (_, Err(error)) => return Err(error.into()),
    }
    Ok(())
}

fn rollback_files(journal: &InstallationJournal) -> Result<()> {
    journal.validate(&journal.product)?;
    for record in journal.records.iter().rev() {
        if record.backup.exists() {
            if record.new_digest.is_some() {
                if record.destination.exists() {
                    validate_private_or_installed_regular_file(&record.destination)?;
                    if Some(file_digest(&record.destination)?) != record.new_digest {
                        bail!(
                            "installed file changed before rollback: {}",
                            record.destination.display()
                        );
                    }
                    fs::remove_file(&record.destination)?;
                } else if !record.staged.exists() {
                    bail!(
                        "new managed destination disappeared before rollback: {}",
                        record.destination.display()
                    );
                }
            } else if record.destination.exists() {
                bail!(
                    "removed managed destination reappeared before rollback: {}",
                    record.destination.display()
                );
            }
            fs::rename(&record.backup, &record.destination)?;
            sync_directory(
                record
                    .destination
                    .parent()
                    .expect("validated destination has a parent"),
            )?;
        } else if let Some(original) = &record.original_digest {
            if !record.destination.exists() || file_digest(&record.destination)? != *original {
                bail!(
                    "original managed destination changed before rollback: {}",
                    record.destination.display()
                );
            }
        } else if record.new_digest.is_some()
            && !record.staged.exists()
            && record.destination.exists()
        {
            validate_private_or_installed_regular_file(&record.destination)?;
            if Some(file_digest(&record.destination)?) != record.new_digest {
                bail!(
                    "new managed destination changed before rollback: {}",
                    record.destination.display()
                );
            }
            fs::remove_file(&record.destination)?;
            sync_directory(
                record
                    .destination
                    .parent()
                    .expect("validated destination has a parent"),
            )?;
        } else if record.new_digest.is_none() && record.destination.exists() {
            bail!(
                "unexpected destination exists for an absent managed file: {}",
                record.destination.display()
            );
        }
    }
    Ok(())
}

fn validate_committed_files(journal: &InstallationJournal) -> Result<()> {
    for record in &journal.records {
        match (
            &record.new_digest,
            fs::symlink_metadata(&record.destination),
        ) {
            (Some(expected), Ok(metadata)) => {
                validate_installed_file(&record.destination, &metadata)?;
                if file_digest(&record.destination)? != *expected {
                    bail!(
                        "accepted managed file changed before recovery: {}",
                        record.destination.display()
                    );
                }
            }
            (Some(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "accepted managed file disappeared before recovery: {}",
                    record.destination.display()
                );
            }
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (None, Ok(_)) => bail!(
                "removed managed file reappeared before recovery: {}",
                record.destination.display()
            ),
            (_, Err(error)) => return Err(error.into()),
        }
    }
    Ok(())
}

fn cleanup_staging(journal: &InstallationJournal) -> Result<()> {
    let mut roots = BTreeSet::new();
    for record in &journal.records {
        roots.insert(
            record
                .staged
                .parent()
                .and_then(Path::parent)
                .expect("validated staged path has a root")
                .to_path_buf(),
        );
    }
    for root in roots {
        if root.exists() {
            validate_root_directory(&root)?;
            fs::remove_dir_all(&root)
                .with_context(|| format!("failed to remove staging tree {}", root.display()))?;
            sync_directory(root.parent().expect("validated stage root has a parent"))?;
        }
    }
    Ok(())
}

fn plan_uninstallation_record(
    product: &ManagedProduct,
    job: JobId,
    managed: ManagedFile,
) -> Result<UninstallationRecord> {
    let destination = match managed {
        ManagedFile::Binary { destination, .. } | ManagedFile::Text { destination, .. } => {
            destination
        }
    };
    let metadata = fs::symlink_metadata(&destination)
        .with_context(|| format!("managed file is missing: {}", destination.display()))?;
    validate_installed_file(&destination, &metadata)?;
    let parent = destination
        .parent()
        .expect("validated managed destination has a parent");
    let name = destination
        .file_name()
        .expect("validated managed destination has a filename");
    Ok(UninstallationRecord {
        digest: file_digest(&destination)?,
        backup: parent
            .join(format!(".capulus-{}-uninstall-{job}", product.name()))
            .join("old")
            .join(name),
        destination,
    })
}

async fn recover_uninstallation(product: &ManagedProduct) -> Result<()> {
    let journal_path = Path::new(INSTALLATION_STATE_ROOT)
        .join(product.name())
        .join("uninstall.json");
    if !journal_path.exists() {
        return Ok(());
    }
    validate_private_file(&journal_path)?;
    let journal: UninstallationJournal = serde_json::from_slice(&fs::read(&journal_path)?)
        .context("failed to decode interrupted Capulus uninstall journal")?;
    journal.validate(product)?;
    if journal.removal_committed {
        for record in &journal.records {
            if record.destination.exists() {
                bail!(
                    "managed destination reappeared after committed uninstall: {}",
                    record.destination.display()
                );
            }
        }
        SystemdManager.reload_removed_installation().await?;
    } else {
        restore_uninstallation_files(&journal)?;
        SystemdManager
            .restore_installation(
                product,
                &product.installation_manifest().enable_units,
                &journal.previously_enabled,
                true,
                journal
                    .previously_enabled
                    .contains(&product.application_socket_name()),
            )
            .await?;
    }
    cleanup_uninstallation_staging(&journal)?;
    remove_private_file(&journal_path)
}

fn restore_uninstallation_files(journal: &UninstallationJournal) -> Result<()> {
    for (index, record) in journal.records.iter().enumerate().rev() {
        if record.backup.exists() {
            if record.destination.exists() {
                bail!(
                    "managed destination reappeared before uninstall rollback: {}",
                    record.destination.display()
                );
            }
            validate_private_or_installed_regular_file(&record.backup)?;
            if file_digest(&record.backup)? != record.digest {
                bail!(
                    "uninstall backup changed before rollback: {}",
                    record.backup.display()
                );
            }
            fs::rename(&record.backup, &record.destination)?;
            sync_directory(
                record
                    .destination
                    .parent()
                    .expect("validated destination has a parent"),
            )?;
        } else if index < journal.committed_records {
            bail!(
                "uninstall backup is missing for removed file: {}",
                record.destination.display()
            );
        } else {
            let metadata = fs::symlink_metadata(&record.destination)?;
            validate_installed_file(&record.destination, &metadata)?;
            if file_digest(&record.destination)? != record.digest {
                bail!(
                    "managed file changed before uninstall rollback: {}",
                    record.destination.display()
                );
            }
        }
    }
    Ok(())
}

fn cleanup_uninstallation_staging(journal: &UninstallationJournal) -> Result<()> {
    let roots = journal
        .records
        .iter()
        .map(|record| {
            record
                .backup
                .parent()
                .and_then(Path::parent)
                .expect("validated uninstall backup has a root")
                .to_path_buf()
        })
        .collect::<BTreeSet<_>>();
    for root in roots {
        if root.exists() {
            validate_root_directory(&root)?;
            fs::remove_dir_all(&root).with_context(|| {
                format!("failed to remove uninstall staging tree {}", root.display())
            })?;
            sync_directory(root.parent().expect("uninstall stage root has a parent"))?;
        }
    }
    Ok(())
}

fn write_uninstallation_journal(path: &Path, journal: &UninstallationJournal) -> Result<()> {
    crate::store::atomic_write(
        path,
        &serde_json::to_vec(journal)?,
        Some(0o600),
        Some(0o700),
    )
    .context("failed to persist Capulus uninstall journal")
}

fn write_journal(path: &Path, journal: &InstallationJournal) -> Result<()> {
    crate::store::atomic_write(
        path,
        &serde_json::to_vec(journal)?,
        Some(0o600),
        Some(0o700),
    )
    .context("failed to persist Capulus installation journal")
}

fn validate_installed_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.gid() != 0 {
        bail!(
            "installed managed path is not a root-owned regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_private_or_installed_regular_file(path: &Path) -> Result<()> {
    validate_installed_file(path, &fs::symlink_metadata(path)?)
}

fn validate_private_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_installed_file(path, &metadata)?;
    if metadata.mode() & 0o077 != 0 {
        bail!(
            "Capulus private file is accessible by other users: {}",
            path.display()
        );
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<String> {
    reader_digest(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?,
    )
}

fn bytes_digest(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn reader_digest(mut file: File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(hex_digest(&digest.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn ensure_root_directory(path: &Path, mode: u32) -> Result<()> {
    validate_normal_absolute(path)?;
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => validate_directory_metadata(&current, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .with_context(|| format!("failed to create {}", current.display()))?;
                validate_root_directory(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn validate_root_directory(path: &Path) -> Result<()> {
    validate_directory_metadata(path, &fs::symlink_metadata(path)?)
}

fn validate_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_dir() || metadata.uid() != 0 || metadata.gid() != 0 {
        bail!(
            "managed parent is not a root-owned real directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_normal_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "managed path is not normalized and absolute: {}",
            path.display()
        );
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_false(value: &bool) -> bool {
    !value
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Into::into)
}

fn managed_destination(managed: &ManagedFile) -> &Path {
    match managed {
        ManagedFile::Binary { destination, .. } | ManagedFile::Text { destination, .. } => {
            destination
        }
    }
}

fn acquire_installation_lock(product: &ManagedProduct) -> Result<crate::InvocationLock> {
    ensure_root_directory(Path::new(INSTALLATION_LOCK_ROOT), 0o700)?;
    crate::acquire_named_in(
        INSTALLATION_LOCK_ROOT,
        &format!("installation-{}", product.name()),
        false,
    )
    .context("failed to acquire product installation lock")
}

fn ensure_installation_state_directory(product: &ManagedProduct) -> Result<PathBuf> {
    ensure_root_directory(Path::new("/var/lib/capulus"), 0o700)?;
    ensure_root_directory(Path::new(INSTALLATION_STATE_ROOT), 0o700)?;
    let state_directory = Path::new(INSTALLATION_STATE_ROOT).join(product.name());
    ensure_root_directory(&state_directory, 0o700)?;
    Ok(state_directory)
}

fn remove_private_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            sync_directory(path.parent().expect("private file has a parent"))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_install_journal() -> InstallationJournal {
        let job = JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        InstallationJournal {
            product: "auc".to_string(),
            job,
            previously_enabled: Vec::new(),
            target_enable_units: vec!["auc-agent.service".to_string()],
            previous_service_file: false,
            committed_records: 0,
            accepted: false,
            records: vec![
                InstallationRecord {
                    destination: PathBuf::from("/usr/local/bin/auc-agent"),
                    staged: PathBuf::from(format!(
                        "/usr/local/bin/.capulus-auc-{job}/new/auc-agent"
                    )),
                    backup: PathBuf::from(format!(
                        "/usr/local/bin/.capulus-auc-{job}/old/auc-agent"
                    )),
                    original_digest: None,
                    new_digest: Some("00".repeat(32)),
                },
                InstallationRecord {
                    destination: PathBuf::from("/etc/systemd/system/auc-agent.service"),
                    staged: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/new/auc-agent.service"
                    )),
                    backup: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/old/auc-agent.service"
                    )),
                    original_digest: None,
                    new_digest: Some("11".repeat(32)),
                },
            ],
        }
    }

    #[test]
    fn journal_paths_are_fixed_to_the_product_and_job() {
        let journal = first_install_journal();
        journal.validate("auc").unwrap();
        let bridge_json = serde_json::to_value(&journal).unwrap();
        let bridge_fields = bridge_json.as_object().unwrap();
        assert!(!bridge_fields.contains_key("accepted"));
        assert!(!bridge_fields.contains_key("previous_application_socket_file"));
        assert!(bridge_fields["records"][0]["new_digest"].as_str().is_some());

        let mut unsafe_journal = journal;
        unsafe_journal.records[0].backup = PathBuf::from("/tmp/auc-agent");
        assert!(unsafe_journal.validate("auc").is_err());
    }

    #[test]
    fn accepted_installation_journal_requires_every_file_commit() {
        let job = JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        let mut journal = InstallationJournal {
            product: "auc".to_string(),
            job,
            previously_enabled: Vec::new(),
            target_enable_units: vec!["auc-agent.service".to_string()],
            previous_service_file: false,
            committed_records: 0,
            accepted: true,
            records: vec![InstallationRecord {
                destination: PathBuf::from("/etc/systemd/system/auc-agent.service"),
                staged: PathBuf::from(format!(
                    "/etc/systemd/system/.capulus-auc-{job}/new/auc-agent.service"
                )),
                backup: PathBuf::from(format!(
                    "/etc/systemd/system/.capulus-auc-{job}/old/auc-agent.service"
                )),
                original_digest: None,
                new_digest: Some("00".repeat(32)),
            }],
        };

        assert!(journal.validate("auc").is_err());
        journal.committed_records = journal.records.len();
        journal.validate("auc").unwrap();
    }

    #[test]
    fn journal_accepts_a_transactional_managed_file_removal() {
        let job = JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        let journal = InstallationJournal {
            product: "auc".to_string(),
            job,
            previously_enabled: vec![
                "auc-agent.service".to_string(),
                "auc-agent.socket".to_string(),
            ],
            target_enable_units: vec!["auc-agent.service".to_string()],
            previous_service_file: true,
            committed_records: 0,
            accepted: false,
            records: vec![
                InstallationRecord {
                    destination: PathBuf::from("/etc/systemd/system/auc-agent.service"),
                    staged: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/new/auc-agent.service"
                    )),
                    backup: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/old/auc-agent.service"
                    )),
                    original_digest: Some("00".repeat(32)),
                    new_digest: Some("11".repeat(32)),
                },
                InstallationRecord {
                    destination: PathBuf::from("/etc/systemd/system/auc-agent.socket"),
                    staged: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/new/auc-agent.socket"
                    )),
                    backup: PathBuf::from(format!(
                        "/etc/systemd/system/.capulus-auc-{job}/old/auc-agent.socket"
                    )),
                    original_digest: Some("22".repeat(32)),
                    new_digest: None,
                },
            ],
        };

        journal.validate("auc").unwrap();
    }

    #[test]
    fn unit_verification_uses_committed_destination() {
        assert_eq!(
            committed_unit_paths(&first_install_journal()),
            [Path::new("/etc/systemd/system/auc-agent.service")]
        );
    }
}
