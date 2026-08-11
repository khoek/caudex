use std::future::Future;
use std::sync::Arc;

use anyhow::Result;

use super::{
    AgentInfo, ErrorCode, ManagedProduct, ManagementHandler, ManagementRequest, ManagementResponse,
    PeerCredentials, ProtocolError, RedeployCoordinator, ResolvedRelease, SystemInstallation,
    VersionTarget,
};

pub trait ReleaseSource: Send + Sync + 'static {
    fn resolve(
        &self,
        target: VersionTarget,
    ) -> impl Future<Output = Result<ResolvedRelease>> + Send;
}

pub struct ManagedAgent<S> {
    product: Arc<ManagedProduct>,
    releases: Arc<S>,
    coordinator: RedeployCoordinator,
}

impl<S> ManagedAgent<S> {
    pub fn new(product: Arc<ManagedProduct>, releases: Arc<S>) -> Result<Self> {
        let coordinator = RedeployCoordinator::new(Arc::clone(&product))?;
        coordinator.start_startup_reconciler()?;
        Ok(Self {
            coordinator,
            product,
            releases,
        })
    }

    pub fn product(&self) -> &ManagedProduct {
        &self.product
    }
}

impl<S: ReleaseSource> ManagementHandler for ManagedAgent<S> {
    async fn handle(
        &self,
        peer: PeerCredentials,
        request: ManagementRequest,
    ) -> Result<ManagementResponse, ProtocolError> {
        match request {
            ManagementRequest::Info => Ok(ManagementResponse::Info(AgentInfo {
                product: self.product.name().to_string(),
                package: self.product.package().to_string(),
                version: self.product.version().to_string(),
                protocol_major: super::PROTOCOL_MAJOR,
            })),
            ManagementRequest::Resolve { target } => self
                .releases
                .resolve(target)
                .await
                .map(|release| ManagementResponse::Resolved {
                    version: release.version.to_string(),
                })
                .map_err(unavailable),
            ManagementRequest::Redeploy {
                target,
                reinstall_requesting_user,
            } => {
                let authorization = self
                    .coordinator
                    .authorize_redeploy(peer, reinstall_requesting_user)
                    .map_err(unauthorized)?;
                let release = self.releases.resolve(target).await.map_err(unavailable)?;
                authorization
                    .validate_release(&self.product, &release)
                    .map_err(unauthorized)?;
                self.coordinator
                    .schedule(authorization, release)
                    .await
                    .map(ManagementResponse::Redeploy)
                    .map_err(conflict)
            }
            ManagementRequest::JobStatus { job } => {
                match self.coordinator.reconciled_status(job).await {
                    Ok(status) => Ok(ManagementResponse::Job(status)),
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        Err(ProtocolError::new(
                            ErrorCode::NotFound,
                            "redeploy job was not found",
                        ))
                    }
                    Err(error) => Err(internal(error)),
                }
            }
            ManagementRequest::Repair => {
                self.coordinator
                    .authorize_repair(peer)
                    .map_err(unauthorized)?;
                SystemInstallation::inspect_and_repair(&self.product)
                    .await
                    .map(ManagementResponse::Repair)
                    .map_err(internal)
            }
            ManagementRequest::Unknown => Err(ProtocolError::new(
                ErrorCode::BadRequest,
                "management method is not supported",
            )),
        }
    }
}

fn unavailable(error: anyhow::Error) -> ProtocolError {
    eprintln!("capulus release resolution failed: {error:#}");
    ProtocolError::new(
        ErrorCode::Unavailable,
        "the requested release could not be resolved",
    )
}

fn conflict(error: anyhow::Error) -> ProtocolError {
    eprintln!("capulus redeploy scheduling failed: {error:#}");
    ProtocolError::new(
        ErrorCode::Conflict,
        "the managed redeploy could not be scheduled",
    )
}

fn unauthorized(error: anyhow::Error) -> ProtocolError {
    eprintln!("capulus rejected a managed-operation caller: {error:#}");
    ProtocolError::new(
        ErrorCode::Unauthorized,
        "the caller is not authorized to modify this managed product",
    )
}

fn internal(error: anyhow::Error) -> ProtocolError {
    eprintln!("capulus managed operation failed: {error:#}");
    ProtocolError::new(ErrorCode::Internal, "managed operation failed")
}
