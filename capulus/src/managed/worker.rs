use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::{
    AgentInfo, JobId, JobPhase, ManagedBuild, ManagedProduct, ManagementClient,
    ManagementClientOptions, ManagementRequest, ManagementResponse, RedeployCoordinator,
    SystemInstallation, reinstall_user_cli,
};

const READINESS_TIMEOUT: Duration = Duration::from_secs(60);
const READINESS_RETRY: Duration = Duration::from_secs(1);

pub struct RedeployWorker {
    product: Arc<ManagedProduct>,
    coordinator: RedeployCoordinator,
}

impl RedeployWorker {
    pub fn new(product: Arc<ManagedProduct>) -> Result<Self> {
        Ok(Self {
            coordinator: RedeployCoordinator::new(Arc::clone(&product))?,
            product,
        })
    }

    pub async fn run<F>(&self, job: JobId, application_health: F) -> Result<()>
    where
        F: Fn() -> Result<AgentInfo>,
    {
        match self.execute(job, application_health).await {
            Ok(required_user_reinstalled) => {
                self.coordinator.complete(job, required_user_reinstalled)?;
                Ok(())
            }
            Err(failure) => {
                self.coordinator.fail(
                    job,
                    format!("{:#}", failure.error),
                    failure.system_committed,
                    failure.rollback_succeeded,
                )?;
                Err(failure.error)
            }
        }
    }

    async fn execute<F>(
        &self,
        job: JobId,
        application_health: F,
    ) -> Result<Option<bool>, WorkerFailure>
    where
        F: Fn() -> Result<AgentInfo>,
    {
        let request = self
            .coordinator
            .take_worker_request(job)
            .map_err(WorkerFailure::before_commit)?;
        self.update(job, JobPhase::Preparing, "preparing managed build", false)
            .map_err(WorkerFailure::before_commit)?;
        let build = ManagedBuild::prepare(request.clone()).map_err(WorkerFailure::before_commit)?;
        self.update(
            job,
            JobPhase::Toolchain,
            "ensuring shared stable Rust toolchain",
            false,
        )
        .map_err(WorkerFailure::before_commit)?;
        build
            .ensure_toolchain()
            .map_err(WorkerFailure::before_commit)?;
        self.update(
            job,
            JobPhase::Building,
            "building exact Cargo release",
            false,
        )
        .map_err(WorkerFailure::before_commit)?;
        let artifacts = build
            .compile(&self.product)
            .map_err(WorkerFailure::before_commit)?;
        self.update(
            job,
            JobPhase::Validating,
            "validated artifact ownership, version, and manifest",
            false,
        )
        .map_err(WorkerFailure::before_commit)?;
        self.update(
            job,
            JobPhase::Staging,
            "staging recoverable system transaction",
            false,
        )
        .map_err(WorkerFailure::before_commit)?;
        let mut installation =
            SystemInstallation::prepare(&self.product, job, &artifacts, &build.account().account)
                .await
                .map_err(WorkerFailure::before_commit)?;
        self.update(
            job,
            JobPhase::CommittingSystem,
            "committing managed system files",
            false,
        )
        .map_err(WorkerFailure::before_commit)?;
        if let Err(error) = installation.commit_files() {
            return Err(rollback_failure(&mut installation, error).await);
        }
        if let Err(error) = installation.activate().await {
            return Err(rollback_failure(&mut installation, error).await);
        }
        if let Err(error) = self.update(
            job,
            JobPhase::RestartingAgent,
            "waiting for both agent protocols to become healthy",
            true,
        ) {
            return Err(rollback_failure(&mut installation, error).await);
        }
        if let Err(error) = self.wait_until_healthy(&request.release.version, &application_health) {
            return Err(rollback_failure(&mut installation, error).await);
        }
        let required_user_reinstalled = if let Some(context) = &request.required_user {
            if let Err(error) = self.update(
                job,
                JobPhase::ReinstallingUser,
                "reinstalling requesting user's existing Cargo CLI",
                true,
            ) {
                return Err(rollback_failure(&mut installation, error).await);
            }
            if let Err(error) = reinstall_user_cli(&self.product, job, &request.release, context) {
                let user_error = error
                    .context("system redeploy succeeded but required user CLI reinstall failed");
                return match installation.finalize() {
                    Ok(()) => Err(WorkerFailure {
                        error: user_error,
                        system_committed: true,
                        rollback_succeeded: None,
                    }),
                    Err(finalize) if installation.acceptance_committed() => Err(WorkerFailure {
                        error: anyhow!(
                            "{user_error:#}; committed installation cleanup failed: {finalize:#}"
                        ),
                        system_committed: true,
                        rollback_succeeded: None,
                    }),
                    Err(finalize) => Err(rollback_failure(
                        &mut installation,
                        anyhow!("{user_error:#}; accepting the installation failed: {finalize:#}"),
                    )
                    .await),
                };
            }
            Some(true)
        } else {
            None
        };
        if let Err(error) = installation.finalize() {
            if installation.acceptance_committed() {
                return Err(WorkerFailure::after_commit(error));
            }
            return Err(rollback_failure(&mut installation, error).await);
        }
        Ok(required_user_reinstalled)
    }

    fn update(
        &self,
        job: JobId,
        phase: JobPhase,
        detail: &'static str,
        system_committed: bool,
    ) -> Result<()> {
        self.coordinator
            .update(job, phase, detail, system_committed)
    }

    fn wait_until_healthy<F>(
        &self,
        expected: &semver::Version,
        application_health: &F,
    ) -> Result<()>
    where
        F: Fn() -> Result<AgentInfo>,
    {
        let mut options = ManagementClientOptions::new(self.product.management_socket_path());
        options.timeout = Duration::from_secs(2);
        let management = ManagementClient::new(options);
        let started = Instant::now();
        let mut last_error = None;
        while started.elapsed() < READINESS_TIMEOUT {
            let result = management
                .request(ManagementRequest::Info)
                .map_err(anyhow::Error::from)
                .and_then(|response| match response {
                    ManagementResponse::Info(info) => validate_health(
                        &info,
                        self.product.name(),
                        self.product.package(),
                        expected,
                    ),
                    _ => Err(anyhow!(
                        "management health request returned the wrong response"
                    )),
                })
                .and_then(|_| application_health())
                .and_then(|info| {
                    validate_health(&info, self.product.name(), self.product.package(), expected)
                });
            match result {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
            thread::sleep(READINESS_RETRY);
        }
        Err(last_error
            .unwrap_or_else(|| anyhow!("agent readiness deadline elapsed without a response"))
            .context("new agent did not become healthy on both sockets"))
    }
}

struct WorkerFailure {
    error: anyhow::Error,
    system_committed: bool,
    rollback_succeeded: Option<bool>,
}

impl WorkerFailure {
    fn before_commit(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            system_committed: false,
            rollback_succeeded: None,
        }
    }

    fn after_commit(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            system_committed: true,
            rollback_succeeded: None,
        }
    }
}

async fn rollback_failure(
    installation: &mut SystemInstallation,
    error: anyhow::Error,
) -> WorkerFailure {
    match installation.rollback().await {
        Ok(()) => WorkerFailure {
            error: error.context("managed installation failed and was rolled back"),
            system_committed: false,
            rollback_succeeded: Some(true),
        },
        Err(rollback) => WorkerFailure {
            error: anyhow!(
                "managed installation failed: {error:#}; rollback also failed: {rollback:#}"
            ),
            system_committed: installation.files_committed(),
            rollback_succeeded: Some(false),
        },
    }
}

fn validate_health(
    info: &AgentInfo,
    product: &str,
    package: &str,
    expected: &semver::Version,
) -> Result<()> {
    if info.product != product
        || info.package != package
        || info.version != expected.to_string()
        || info.protocol_major != super::PROTOCOL_MAJOR
    {
        return Err(anyhow!(
            "agent health identity/version does not match {product} {expected}"
        ));
    }
    Ok(())
}
