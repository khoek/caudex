use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::account::ensure_root_directory;
use super::systemd::SystemdError;
use super::user_install::{CleanupDisposition, cleanup_inactive_user_installations};
use super::{
    JobId, JobPhase, ManagedProduct, PeerCredentials, RedeployJob, RedeployOutcome,
    ResolvedRelease, SystemdManager, UserInstallContext,
};

const STATE_ROOT: &str = "/var/lib/capulus/jobs";
const RUNTIME_ROOT: &str = "/run/capulus/jobs";
const LOCK_ROOT: &str = "/run/capulus/locks";
const TRANSIENT_START_GRACE: Duration = Duration::from_secs(5);
const ACTIVE_RECONCILIATION_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedeployRequest {
    pub job: JobId,
    pub product: String,
    pub package: String,
    pub release: ResolvedRelease,
    pub required_user: Option<UserInstallContext>,
}

impl std::fmt::Debug for RedeployRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RedeployRequest")
            .field("job", &self.job)
            .field("product", &self.product)
            .field("package", &self.package)
            .field("version", &self.release.version)
            .field(
                "required_user",
                &self.required_user.as_ref().map(|user| user.uid),
            )
            .finish_non_exhaustive()
    }
}

impl RedeployRequest {
    fn validate(&self, product: &ManagedProduct) -> Result<()> {
        if self.product != product.name() || self.package != product.package() {
            bail!("redeploy request does not match the installed managed product");
        }
        self.release.validate()?;
        if let Some(user) = &self.required_user {
            user.revalidate(&product.user_binary().cargo_name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RedeployCoordinator {
    product: Arc<ManagedProduct>,
    store: JobStore,
    systemd: SystemdManager,
    startup_reconciliation: StartupReconciliation,
}

impl RedeployCoordinator {
    pub fn new(product: Arc<ManagedProduct>) -> Result<Self> {
        let store = JobStore::new(product.name())?;
        let active = store.active()?;
        let status = active
            .as_ref()
            .map(|active| store.status(active.job))
            .transpose()?;
        let startup_reconciliation = match startup_disposition(active.as_ref().zip(status.as_ref()))
        {
            StartupDisposition::Cleanup => cleanup_inactive_user_installations()?.into(),
            StartupDisposition::ClearAndCleanup(job) => {
                store.clear_active(job)?;
                cleanup_inactive_user_installations()?.into()
            }
            StartupDisposition::Monitor(job) => StartupReconciliation::Monitor(job),
        };
        Ok(Self {
            store,
            product,
            systemd: SystemdManager,
            startup_reconciliation,
        })
    }

    pub(super) fn start_startup_reconciler(&self) -> Result<()> {
        if self.startup_reconciliation == StartupReconciliation::None {
            return Ok(());
        }
        let coordinator = self.clone();
        std::thread::Builder::new()
            .name(format!("{}-capulus-reconcile", self.product.name()))
            .spawn(move || {
                if let Err(error) = coordinator.run_startup_reconciliation() {
                    eprintln!("capulus startup reconciliation failed: {error:#}");
                }
            })
            .context("failed to start Capulus startup reconciler")?;
        Ok(())
    }

    fn run_startup_reconciliation(&self) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create the Capulus startup-reconciliation runtime")?
            .block_on(async {
                match self.startup_reconciliation {
                    StartupReconciliation::None => Ok(()),
                    StartupReconciliation::Cleanup => cleanup_until_available().await,
                    StartupReconciliation::Monitor(job) => self.monitor_active_job_async(job).await,
                }
            })
    }

    pub(super) fn authorize_redeploy(
        &self,
        peer: PeerCredentials,
        reinstall_requesting_user: bool,
    ) -> Result<RedeployAuthorization> {
        if peer.uid == 0 {
            return Ok(RedeployAuthorization {
                root: true,
                required_user: None,
            });
        }
        if !reinstall_requesting_user {
            bail!("non-root redeploys must reinstall the requesting user's Cargo CLI");
        }
        let required_user =
            UserInstallContext::capture(peer, &self.product.user_binary().cargo_name)?.ok_or_else(
                || anyhow!("requesting user has no existing Cargo-installed managed CLI"),
            )?;
        Ok(RedeployAuthorization {
            root: false,
            required_user: Some(required_user),
        })
    }

    pub(super) fn authorize_repair(&self, peer: PeerCredentials) -> Result<()> {
        if peer.uid == 0 {
            return Ok(());
        }
        UserInstallContext::capture(peer, &self.product.user_binary().cargo_name)?.ok_or_else(
            || anyhow!("requesting user has no existing Cargo-installed managed CLI"),
        )?;
        Ok(())
    }

    pub(super) async fn schedule(
        &self,
        authorization: RedeployAuthorization,
        release: ResolvedRelease,
    ) -> Result<RedeployOutcome> {
        release.validate()?;
        if !authorization.root && release.version < *self.product.version() {
            bail!("non-root callers cannot downgrade a managed system installation");
        }
        let _lock = capulus_lock(self.product.name())?;
        self.reconcile_and_arm_cleanup().await?;
        if let Some(active) = self.store.active()? {
            let status = self.store.status(active.job)?;
            if !status.phase.is_terminal() {
                if active.version == release.version
                    && active.required_uid
                        == authorization.required_user.as_ref().map(|user| user.uid)
                {
                    return Ok(RedeployOutcome {
                        job: active.job,
                        unit: status.unit,
                        version: active.version.to_string(),
                        started: false,
                    });
                }
                bail!(
                    "redeploy {} is already active for version {}",
                    active.job,
                    active.version
                );
            }
            self.store.clear_active(active.job)?;
        }
        let job = JobId::random();
        let unit = SystemdManager::redeploy_unit_name(&self.product, job);
        let request = RedeployRequest {
            job,
            product: self.product.name().to_string(),
            package: self.product.package().to_string(),
            release,
            required_user: authorization.required_user,
        };
        request.validate(&self.product)?;
        self.store.write_request(&request)?;
        self.store.initialize(&request, unit.clone())?;
        if let Err(error) = self.systemd.start_redeploy(&self.product, job).await {
            self.store.fail(
                job,
                format!("failed to start transient unit: {error}"),
                false,
                None,
                None,
            )?;
            self.store.remove_request(job)?;
            return Err(error.into());
        }
        self.spawn_active_reconciler(job);
        Ok(RedeployOutcome {
            job,
            unit,
            version: request.release.version.to_string(),
            started: true,
        })
    }

    fn spawn_active_reconciler(&self, job: JobId) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            if let Err(error) = coordinator.monitor_active_job_async(job).await {
                eprintln!("capulus active-job reconciliation failed: {error:#}");
            }
        });
    }

    fn spawn_cleanup_reconciler(&self) {
        tokio::spawn(async {
            if let Err(error) = cleanup_until_available().await {
                eprintln!("capulus user-install cleanup reconciliation failed: {error:#}");
            }
        });
    }

    async fn monitor_active_job_async(&self, job: JobId) -> Result<()> {
        loop {
            match monitored_job_state(self.store.active()?.as_ref(), job) {
                MonitoredJobState::Active => {}
                MonitoredJobState::Inactive => return cleanup_until_available().await,
                MonitoredJobState::Superseded => return Ok(()),
            }
            match self.reconcile_active().await {
                Ok(_) => {}
                Err(error) if error.downcast_ref::<SystemdError>().is_some() => {
                    eprintln!("capulus systemd reconciliation failed; retrying: {error:#}");
                }
                Err(error) => return Err(error),
            }
            match monitored_job_state(self.store.active()?.as_ref(), job) {
                MonitoredJobState::Active => {
                    tokio::time::sleep(ACTIVE_RECONCILIATION_INTERVAL).await;
                }
                MonitoredJobState::Inactive => return cleanup_until_available().await,
                MonitoredJobState::Superseded => return Ok(()),
            }
        }
    }

    pub fn status(&self, job: JobId) -> Result<RedeployJob> {
        self.store.status(job)
    }

    pub(super) async fn reconciled_status(&self, job: JobId) -> Result<RedeployJob> {
        if self.store.active()?.is_some_and(|active| active.job == job) {
            self.reconcile_and_arm_cleanup().await?;
        }
        self.store.status(job)
    }

    pub fn active(&self) -> Result<Option<RedeployJob>> {
        self.store
            .active()?
            .map(|active| self.store.status(active.job))
            .transpose()
    }

    pub async fn reconciled_active(&self) -> Result<Option<RedeployJob>> {
        self.reconcile_and_arm_cleanup().await?;
        self.active()
    }

    pub fn take_worker_request(&self, job: JobId) -> Result<RedeployRequest> {
        let request = self.store.take_request(job)?;
        request.validate(&self.product)?;
        Ok(request)
    }

    pub fn update(
        &self,
        job: JobId,
        phase: JobPhase,
        detail: impl Into<String>,
        system_committed: bool,
    ) -> Result<()> {
        self.store
            .update(job, phase, detail.into(), system_committed)
    }

    pub fn fail(
        &self,
        job: JobId,
        detail: impl Into<String>,
        system_committed: bool,
        rollback_succeeded: Option<bool>,
    ) -> Result<()> {
        self.store.fail(
            job,
            detail.into(),
            system_committed,
            rollback_succeeded,
            None,
        )
    }

    pub(super) fn fail_worker(
        &self,
        job: JobId,
        detail: impl Into<String>,
        system_committed: bool,
        rollback_succeeded: Option<bool>,
        required_user_reinstalled: Option<bool>,
    ) -> Result<()> {
        self.store.fail(
            job,
            detail.into(),
            system_committed,
            rollback_succeeded,
            required_user_reinstalled,
        )
    }

    pub(super) fn record_required_user_reinstalled(
        &self,
        job: JobId,
        required_user_reinstalled: bool,
    ) -> Result<()> {
        self.store
            .record_required_user_reinstalled(job, required_user_reinstalled)
    }

    pub fn complete(&self, job: JobId, required_user_reinstalled: Option<bool>) -> Result<()> {
        self.store.complete(job, required_user_reinstalled)
    }

    async fn reconcile_and_arm_cleanup(&self) -> Result<()> {
        if self.reconcile_active().await? == CleanupDisposition::Busy {
            self.spawn_cleanup_reconciler();
        }
        Ok(())
    }

    async fn reconcile_active(&self) -> Result<CleanupDisposition> {
        let Some(active) = self.store.active()? else {
            return cleanup_inactive_user_installations();
        };
        let status = self.store.status(active.job)?;
        if status.phase.is_terminal() {
            self.store.clear_active(active.job)?;
            return cleanup_inactive_user_installations();
        }
        if self
            .systemd
            .redeploy_is_active(&self.product, active.job)
            .await?
            || (status.phase == JobPhase::Queued
                && self.store.request_is_within_start_grace(active.job)?)
        {
            return Ok(CleanupDisposition::Complete);
        }
        let Some(current) = self.store.active()? else {
            return cleanup_inactive_user_installations();
        };
        let status = self.store.status(current.job)?;
        if current.job != active.job || status.phase.is_terminal() {
            return Ok(CleanupDisposition::Complete);
        }
        self.store.fail(
            active.job,
            format!(
                "transient redeploy unit {} stopped before recording a terminal outcome",
                status.unit
            ),
            status.system_committed,
            None,
            None,
        )?;
        self.store.remove_request(active.job)?;
        cleanup_inactive_user_installations()
    }
}

async fn cleanup_until_available() -> Result<()> {
    cleanup_until_available_with(
        cleanup_inactive_user_installations,
        ACTIVE_RECONCILIATION_INTERVAL,
    )
    .await
}

async fn cleanup_until_available_with(
    mut cleanup: impl FnMut() -> Result<CleanupDisposition>,
    retry_interval: Duration,
) -> Result<()> {
    loop {
        match cleanup()? {
            CleanupDisposition::Complete => return Ok(()),
            CleanupDisposition::Busy => tokio::time::sleep(retry_interval).await,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupDisposition {
    Cleanup,
    ClearAndCleanup(JobId),
    Monitor(JobId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StartupReconciliation {
    None,
    Cleanup,
    Monitor(JobId),
}

impl From<CleanupDisposition> for StartupReconciliation {
    fn from(disposition: CleanupDisposition) -> Self {
        match disposition {
            CleanupDisposition::Complete => Self::None,
            CleanupDisposition::Busy => Self::Cleanup,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonitoredJobState {
    Active,
    Inactive,
    Superseded,
}

fn startup_disposition(active: Option<(&ActiveJob, &RedeployJob)>) -> StartupDisposition {
    match active {
        None => StartupDisposition::Cleanup,
        Some((active, status)) if status.phase.is_terminal() => {
            StartupDisposition::ClearAndCleanup(active.job)
        }
        Some((active, _)) => StartupDisposition::Monitor(active.job),
    }
}

fn monitored_job_state(active: Option<&ActiveJob>, job: JobId) -> MonitoredJobState {
    match active {
        Some(active) if active.job == job => MonitoredJobState::Active,
        Some(_) => MonitoredJobState::Superseded,
        None => MonitoredJobState::Inactive,
    }
}

#[derive(Debug)]
pub(super) struct RedeployAuthorization {
    root: bool,
    required_user: Option<UserInstallContext>,
}

impl RedeployAuthorization {
    pub(super) fn validate_release(
        &self,
        product: &ManagedProduct,
        release: &ResolvedRelease,
    ) -> Result<()> {
        if !self.root && release.version < *product.version() {
            bail!("non-root callers cannot downgrade a managed system installation");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct JobStore {
    product: String,
    state_directory: PathBuf,
    runtime_directory: PathBuf,
}

impl JobStore {
    fn new(product: &str) -> Result<Self> {
        if !rustix::process::geteuid().is_root() {
            bail!("Capulus job store requires root");
        }
        let store = Self {
            product: product.to_string(),
            state_directory: Path::new(STATE_ROOT).join(product),
            runtime_directory: Path::new(RUNTIME_ROOT).join(product),
        };
        ensure_root_directory(Path::new("/var/lib/capulus"), 0o700)?;
        ensure_root_directory(Path::new(STATE_ROOT), 0o700)?;
        ensure_root_directory(Path::new("/run/capulus"), 0o711)?;
        ensure_root_directory(Path::new(RUNTIME_ROOT), 0o700)?;
        ensure_root_directory(Path::new(LOCK_ROOT), 0o700)?;
        ensure_root_directory(&store.state_directory, 0o700)?;
        ensure_root_directory(&store.runtime_directory, 0o700)?;
        store.reconcile_boot()?;
        Ok(store)
    }

    fn initialize(&self, request: &RedeployRequest, unit: String) -> Result<()> {
        let status = StoredJob {
            boot_id: boot_id()?,
            status: RedeployJob {
                job: request.job,
                product: request.product.clone(),
                version: request.release.version.to_string(),
                unit,
                phase: JobPhase::Queued,
                detail: "queued for transient systemd execution".to_string(),
                system_committed: false,
                rollback_succeeded: None,
                required_user_reinstalled: None,
            },
        };
        write_json(&self.status_path(request.job), &status, 0o600)?;
        write_json(
            &self.active_path(),
            &ActiveJob {
                job: request.job,
                version: request.release.version.clone(),
                required_uid: request.required_user.as_ref().map(|user| user.uid),
            },
            0o600,
        )
    }

    fn status(&self, job: JobId) -> Result<RedeployJob> {
        let stored: StoredJob = read_json(&self.status_path(job))?;
        if stored.status.job != job || stored.status.product != self.product {
            bail!("stored redeploy status identity does not match its path");
        }
        Ok(stored.status)
    }

    fn active(&self) -> Result<Option<ActiveJob>> {
        match read_json(&self.active_path()) {
            Ok(active) => Ok(Some(active)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn clear_active(&self, job: JobId) -> Result<()> {
        if self.active()?.is_some_and(|active| active.job == job) {
            remove_file(&self.active_path())?;
        }
        Ok(())
    }

    fn write_request(&self, request: &RedeployRequest) -> Result<()> {
        let mut bytes = Vec::new();
        ciborium::into_writer(request, &mut bytes).context("failed to encode redeploy request")?;
        crate::store::atomic_write(&self.request_path(request.job), &bytes, Some(0o600), None)
            .context("failed to persist redeploy request")
    }

    fn take_request(&self, job: JobId) -> Result<RedeployRequest> {
        let path = self.request_path(job);
        validate_private_root_file(&path)?;
        let bytes = fs::read(&path).context("failed to read redeploy request")?;
        remove_file(&path)?;
        ciborium::from_reader(bytes.as_slice()).context("failed to decode redeploy request")
    }

    fn remove_request(&self, job: JobId) -> Result<()> {
        remove_file(&self.request_path(job))
    }

    fn update(
        &self,
        job: JobId,
        phase: JobPhase,
        detail: String,
        system_committed: bool,
    ) -> Result<()> {
        let mut stored: StoredJob = read_json(&self.status_path(job))?;
        validate_transition(&stored.status.phase, &phase)?;
        stored.status.phase = phase;
        stored.status.detail = bounded_detail(detail);
        stored.status.system_committed = system_committed;
        write_json(&self.status_path(job), &stored, 0o600)
    }

    fn fail(
        &self,
        job: JobId,
        detail: String,
        system_committed: bool,
        rollback_succeeded: Option<bool>,
        required_user_reinstalled: Option<bool>,
    ) -> Result<()> {
        let mut stored: StoredJob = read_json(&self.status_path(job))?;
        if stored.status.phase.is_terminal() {
            bail!("cannot fail terminal redeploy job {job}");
        }
        stored.status.required_user_reinstalled = failed_required_user_reinstalled(
            stored.status.required_user_reinstalled,
            required_user_reinstalled,
        );
        stored.status.phase = JobPhase::Failed;
        crate::store::atomic_write(
            &self.diagnostic_path(job),
            bounded_detail(detail).as_bytes(),
            Some(0o600),
            None,
        )
        .context("failed to persist private redeploy diagnostic")?;
        stored.status.detail =
            "managed redeploy failed; inspect the transient unit journal".to_string();
        stored.status.system_committed = system_committed;
        stored.status.rollback_succeeded = rollback_succeeded;
        write_json(&self.status_path(job), &stored, 0o600)?;
        self.clear_active(job)
    }

    fn record_required_user_reinstalled(
        &self,
        job: JobId,
        required_user_reinstalled: bool,
    ) -> Result<()> {
        let mut stored: StoredJob = read_json(&self.status_path(job))?;
        if stored.status.phase != JobPhase::ReinstallingUser || !stored.status.system_committed {
            bail!("cannot record a user reinstall outside the committed user-reinstall phase");
        }
        stored.status.required_user_reinstalled = Some(required_user_reinstalled);
        write_json(&self.status_path(job), &stored, 0o600)
    }

    fn complete(&self, job: JobId, required_user_reinstalled: Option<bool>) -> Result<()> {
        let mut stored: StoredJob = read_json(&self.status_path(job))?;
        validate_transition(&stored.status.phase, &JobPhase::Complete)?;
        stored.status.phase = JobPhase::Complete;
        stored.status.detail = "managed redeploy completed".to_string();
        stored.status.system_committed = true;
        stored.status.required_user_reinstalled = required_user_reinstalled;
        write_json(&self.status_path(job), &stored, 0o600)?;
        self.clear_active(job)
    }

    fn reconcile_boot(&self) -> Result<()> {
        let Some(active) = self.active()? else {
            return Ok(());
        };
        let stored: StoredJob = read_json(&self.status_path(active.job))?;
        if !stored.status.phase.is_terminal() && stored.boot_id != boot_id()? {
            self.fail(
                active.job,
                "redeploy was interrupted by a system reboot".to_string(),
                stored.status.system_committed,
                None,
                None,
            )?;
        }
        Ok(())
    }

    fn request_is_within_start_grace(&self, job: JobId) -> Result<bool> {
        let path = self.request_path(job);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        validate_private_root_file_metadata(&path, &metadata)?;
        Ok(metadata
            .modified()?
            .elapsed()
            .is_ok_and(|age| age < TRANSIENT_START_GRACE))
    }

    fn active_path(&self) -> PathBuf {
        self.state_directory.join("active.json")
    }

    fn status_path(&self, job: JobId) -> PathBuf {
        self.state_directory.join(format!("{job}.json"))
    }

    fn request_path(&self, job: JobId) -> PathBuf {
        self.runtime_directory.join(format!("{job}.cbor"))
    }

    fn diagnostic_path(&self, job: JobId) -> PathBuf {
        self.state_directory.join(format!("{job}.diagnostic"))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredJob {
    boot_id: String,
    status: RedeployJob,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActiveJob {
    job: JobId,
    version: semver::Version,
    required_uid: Option<u32>,
}

fn validate_transition(current: &JobPhase, next: &JobPhase) -> Result<()> {
    if current.is_terminal() {
        bail!("cannot transition a terminal redeploy job");
    }
    if next != &JobPhase::Failed && phase_rank(next) <= phase_rank(current) {
        bail!("invalid redeploy phase transition from {current:?} to {next:?}");
    }
    Ok(())
}

fn phase_rank(phase: &JobPhase) -> u8 {
    match phase {
        JobPhase::Queued => 0,
        JobPhase::Preparing => 1,
        JobPhase::Toolchain => 2,
        JobPhase::Resolving => 3,
        JobPhase::Building => 4,
        JobPhase::Validating => 5,
        JobPhase::Staging => 6,
        JobPhase::CommittingSystem => 7,
        JobPhase::RestartingAgent => 8,
        JobPhase::ReinstallingUser => 9,
        JobPhase::Complete => 10,
        JobPhase::Failed => u8::MAX,
    }
}

fn failed_required_user_reinstalled(
    recorded: Option<bool>,
    explicit: Option<bool>,
) -> Option<bool> {
    explicit.or(recorded)
}

fn capulus_lock(product: &str) -> Result<crate::InvocationLock> {
    crate::acquire_named_in(LOCK_ROOT, &format!("{product}-schedule"), false)
        .context("failed to acquire Capulus scheduling lock")
}

fn write_json<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("failed to encode Capulus job state")?;
    crate::store::atomic_write(path, &bytes, Some(mode), None)
        .with_context(|| format!("failed to persist {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    validate_private_root_file(path)?;
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("failed to decode {}", path.display()))
}

fn validate_private_root_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_private_root_file_metadata(path, &metadata)
}

fn validate_private_root_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_file()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "Capulus state is not a private root-owned regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => File::open(path.parent().expect("job file has a parent"))?
            .sync_all()
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn boot_id() -> Result<String> {
    let value = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("failed to read kernel boot ID")?;
    let value = value.trim();
    if value.len() != 36
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(anyhow!("kernel boot ID is malformed"));
    }
    Ok(value.to_string())
}

fn bounded_detail(mut detail: String) -> String {
    const MAX_DETAIL_CHARS: usize = 2048;
    if detail.chars().count() <= MAX_DETAIL_CHARS {
        return detail;
    }
    detail = detail.chars().take(MAX_DETAIL_CHARS - 1).collect();
    detail.push('…');
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_transitions_are_forward_only() {
        assert!(validate_transition(&JobPhase::Queued, &JobPhase::Building).is_ok());
        assert!(validate_transition(&JobPhase::Building, &JobPhase::Failed).is_ok());
        assert!(validate_transition(&JobPhase::Building, &JobPhase::Preparing).is_err());
        assert!(validate_transition(&JobPhase::Complete, &JobPhase::Failed).is_err());
    }

    #[test]
    fn public_job_detail_is_bounded() {
        let detail = bounded_detail("x".repeat(3000));
        assert_eq!(detail.chars().count(), 2048);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn failed_jobs_preserve_only_explicit_user_reinstall_outcomes() {
        assert_eq!(failed_required_user_reinstalled(None, None), None);
        assert_eq!(
            failed_required_user_reinstalled(Some(true), None),
            Some(true)
        );
        assert_eq!(
            failed_required_user_reinstalled(None, Some(false)),
            Some(false)
        );
    }

    #[test]
    fn coordinator_startup_cleans_terminal_state_without_cleaning_a_live_job() {
        let active = ActiveJob {
            job: JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap(),
            version: semver::Version::new(1, 2, 3),
            required_uid: Some(1000),
        };
        let mut status = RedeployJob {
            job: active.job,
            product: "auc".to_string(),
            version: "1.2.3".to_string(),
            unit: "auc-redeploy.service".to_string(),
            phase: JobPhase::RestartingAgent,
            detail: String::new(),
            system_committed: true,
            rollback_succeeded: None,
            required_user_reinstalled: None,
        };
        assert_eq!(
            startup_disposition(Some((&active, &status))),
            StartupDisposition::Monitor(active.job)
        );
        status.phase = JobPhase::Complete;
        assert_eq!(
            startup_disposition(Some((&active, &status))),
            StartupDisposition::ClearAndCleanup(active.job)
        );
        assert_eq!(startup_disposition(None), StartupDisposition::Cleanup);
        assert_eq!(
            monitored_job_state(Some(&active), active.job),
            MonitoredJobState::Active
        );
        assert_eq!(
            monitored_job_state(
                Some(&active),
                JobId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            ),
            MonitoredJobState::Superseded
        );
        assert_eq!(
            monitored_job_state(None, active.job),
            MonitoredJobState::Inactive
        );
    }

    #[test]
    fn cleanup_retries_after_build_lock_contention() {
        let mut attempts = 0;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(cleanup_until_available_with(
                || {
                    attempts += 1;
                    Ok(if attempts == 1 {
                        CleanupDisposition::Busy
                    } else {
                        CleanupDisposition::Complete
                    })
                },
                Duration::ZERO,
            ))
            .unwrap();

        assert_eq!(attempts, 2);
    }
}
