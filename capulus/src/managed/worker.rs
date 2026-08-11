use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use super::user_install::PreparedUserInstall;
use super::{
    AgentInfo, JobId, JobPhase, ManagedBuild, ManagedProduct, ManagementClient,
    ManagementClientOptions, ManagementRequest, ManagementResponse, RedeployCoordinator,
    SystemInstallation,
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
                self.coordinator.fail_worker(
                    job,
                    format!("{:#}", failure.error),
                    failure.system_committed,
                    failure.rollback_succeeded,
                    failure.required_user_reinstalled,
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
        let (artifacts, mut user_install) = compile_and_prepare_user_install(
            request.required_user.as_ref(),
            || build.compile(&self.product),
            || build.reclaim_target_for_user_install(),
            |context| PreparedUserInstall::prepare(&self.product, job, context),
        )
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
            let mut user_install = user_install
                .take()
                .expect("required user has a prepared Cargo workspace");
            let attempt =
                user_install.reinstall_and_verify(&self.product, &request.release, context);
            let record = self
                .coordinator
                .record_required_user_reinstalled(job, attempt.outcome)
                .context("failed to persist the requesting-user reinstall outcome");
            let cleanup = user_install
                .cleanup()
                .context("failed to clean the requesting-user Cargo workspace");
            if let Err(error) =
                combine_worker_results(combine_worker_results(attempt.result, record), cleanup)
            {
                return Err(finalize_post_commit_failure(
                    &mut installation,
                    error.context("system redeploy succeeded but its user CLI work failed"),
                    Some(attempt.outcome),
                )
                .await);
            }
            debug_assert!(attempt.outcome);
            Some(true)
        } else {
            None
        };
        if let Err(error) = installation.finalize() {
            if installation.acceptance_committed() {
                return Err(WorkerFailure::after_commit(
                    error,
                    required_user_reinstalled,
                ));
            }
            return Err(rollback_failure(&mut installation, error)
                .await
                .with_required_user_reinstalled(required_user_reinstalled));
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

fn compile_and_prepare_user_install<A, U>(
    context: Option<&super::UserInstallContext>,
    compile: impl FnOnce() -> Result<A>,
    reclaim_system_target: impl FnOnce() -> Result<()>,
    prepare: impl FnOnce(&super::UserInstallContext) -> Result<U>,
) -> Result<(A, Option<U>)> {
    let artifacts = compile()?;
    let Some(context) = context else {
        return Ok((artifacts, None));
    };
    reclaim_system_target()?;
    prepare(context).map(|user_install| (artifacts, Some(user_install)))
}

struct WorkerFailure {
    error: anyhow::Error,
    system_committed: bool,
    rollback_succeeded: Option<bool>,
    required_user_reinstalled: Option<bool>,
}

impl WorkerFailure {
    fn before_commit(error: impl Into<anyhow::Error>) -> Self {
        Self {
            error: error.into(),
            system_committed: false,
            rollback_succeeded: None,
            required_user_reinstalled: None,
        }
    }

    fn after_commit(
        error: impl Into<anyhow::Error>,
        required_user_reinstalled: Option<bool>,
    ) -> Self {
        Self {
            error: error.into(),
            system_committed: true,
            rollback_succeeded: None,
            required_user_reinstalled,
        }
    }

    fn with_required_user_reinstalled(mut self, required_user_reinstalled: Option<bool>) -> Self {
        self.required_user_reinstalled = required_user_reinstalled;
        self
    }
}

async fn finalize_post_commit_failure(
    installation: &mut SystemInstallation,
    error: anyhow::Error,
    required_user_reinstalled: Option<bool>,
) -> WorkerFailure {
    match installation.finalize() {
        Ok(()) => WorkerFailure::after_commit(error, required_user_reinstalled),
        Err(finalize) if installation.acceptance_committed() => WorkerFailure::after_commit(
            anyhow!("{error:#}; committed installation cleanup failed: {finalize:#}"),
            required_user_reinstalled,
        ),
        Err(finalize) => rollback_failure(
            installation,
            anyhow!("{error:#}; accepting the installation failed: {finalize:#}"),
        )
        .await
        .with_required_user_reinstalled(required_user_reinstalled),
    }
}

fn combine_worker_results(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(anyhow!("{first:#}; additionally: {second:#}")),
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
            required_user_reinstalled: None,
        },
        Err(rollback) => WorkerFailure {
            error: anyhow!(
                "managed installation failed: {error:#}; rollback also failed: {rollback:#}"
            ),
            system_committed: installation.files_committed(),
            rollback_succeeded: Some(false),
            required_user_reinstalled: None,
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;
    use crate::managed::UserInstallContext;

    #[test]
    fn required_user_target_is_reclaimed_before_workspace_preparation() {
        let events = RefCell::new(Vec::new());
        let context = test_context();
        let prepared = compile_and_prepare_user_install(
            Some(&context),
            || {
                events.borrow_mut().push("compile");
                Ok("artifacts")
            },
            || {
                events.borrow_mut().push("reclaim");
                Ok(())
            },
            |_| {
                events.borrow_mut().push("prepare");
                Ok(0xDEADBEEF_u32)
            },
        )
        .unwrap();
        events.borrow_mut().push("system-commit");

        assert_eq!(prepared, ("artifacts", Some(0xDEADBEEF)));
        assert_eq!(
            *events.borrow(),
            ["compile", "reclaim", "prepare", "system-commit"]
        );
    }

    #[test]
    fn jobs_without_a_required_user_retain_the_shared_target() {
        let called = RefCell::new(false);
        let prepared = compile_and_prepare_user_install::<_, ()>(
            None,
            || Ok("artifacts"),
            || {
                *called.borrow_mut() = true;
                Ok(())
            },
            |_| unreachable!(),
        )
        .unwrap();

        assert_eq!(prepared, ("artifacts", None));
        assert!(!*called.borrow());
    }

    fn test_context() -> UserInstallContext {
        UserInstallContext {
            account_name: "example".to_string(),
            uid: 1000,
            gid: 1000,
            home: PathBuf::from("/home/example"),
            cargo_home: PathBuf::from("/home/example/.cargo"),
            rustup_home: PathBuf::from("/home/example/.rustup"),
            cargo: PathBuf::from("/home/example/.cargo/bin/cargo"),
            rustup: PathBuf::from("/home/example/.cargo/bin/rustup"),
            installed_binary: PathBuf::from("/home/example/.cargo/bin/auc"),
        }
    }
}
