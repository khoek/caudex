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
        Ok(Self {
            coordinator: RedeployCoordinator::new(Arc::clone(&product))?,
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
                let release = self.releases.resolve(target).await.map_err(unavailable)?;
                self.coordinator
                    .schedule(peer, release, reinstall_requesting_user)
                    .await
                    .map(ManagementResponse::Redeploy)
                    .map_err(conflict)
            }
            ManagementRequest::JobStatus { job } => self
                .coordinator
                .status(job)
                .map(ManagementResponse::Job)
                .map_err(|_| ProtocolError::new(ErrorCode::NotFound, "redeploy job was not found")),
            ManagementRequest::Repair => SystemInstallation::inspect_and_repair(&self.product)
                .await
                .map(ManagementResponse::Repair)
                .map_err(internal),
            ManagementRequest::Unknown => Err(ProtocolError::new(
                ErrorCode::BadRequest,
                "management method is not supported",
            )),
        }
    }
}

fn unavailable(error: anyhow::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        format!("release resolution failed: {error}"),
    )
}

fn conflict(error: anyhow::Error) -> ProtocolError {
    ProtocolError::new(ErrorCode::Conflict, error.to_string())
}

fn internal(error: anyhow::Error) -> ProtocolError {
    eprintln!("Capulus managed operation failed: {error:#}");
    ProtocolError::new(ErrorCode::Internal, "managed operation failed")
}
