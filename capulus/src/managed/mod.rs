//! Linux managed-system support for privileged product agents.

#[cfg(feature = "managed-system")]
mod account;
#[cfg(feature = "managed-system")]
mod activation;
#[cfg(feature = "managed-system")]
mod agent;
#[cfg(feature = "managed-system")]
mod build;
mod client;
#[cfg(feature = "managed-system")]
mod installation;
#[cfg(feature = "managed-system")]
mod job;
#[cfg(feature = "managed-system")]
mod product;
mod protocol;
#[cfg(feature = "managed-system")]
mod release;
#[cfg(feature = "managed-system")]
mod server;
#[cfg(feature = "managed-system")]
mod systemd;
#[cfg(feature = "managed-system")]
mod user_install;
#[cfg(feature = "managed-system")]
mod worker;

#[cfg(feature = "managed-system")]
pub use account::{BuildAccount, UnixAccount, UserInstallContext};
#[cfg(feature = "managed-system")]
pub use activation::{ActivatedListeners, ActivationError};
#[cfg(feature = "managed-system")]
pub use agent::{ManagedAgent, ReleaseSource};
#[cfg(feature = "managed-system")]
pub use build::{BuildArtifacts, ManagedBuild};
pub use client::{ManagementClient, ManagementClientOptions};
#[cfg(feature = "managed-system")]
pub use installation::{SystemInstallation, SystemUninstallation};
#[cfg(feature = "managed-system")]
pub use job::{RedeployCoordinator, RedeployRequest};
#[cfg(feature = "managed-system")]
pub use product::{
    AgentServiceOptions, ApplicationSocketOptions, InstallationManifest, ManagedFile,
    ManagedProduct, ManagedProductOptions, ProductValidationError, ServiceHardening, SocketOptions,
    SystemBinary, UserBinary,
};
pub use protocol::{
    AgentInfo, ErrorCode, JobId, JobPhase, ManagementError, ManagementRequest, ManagementResponse,
    PROTOCOL_MAJOR, PeerCredentials, ProtocolError, RedeployJob, RedeployOutcome, RepairOutcome,
    RequestId, VersionTarget,
};
#[cfg(feature = "managed-system")]
pub use release::{CargoRegistry, ResolvedRelease};
#[cfg(feature = "managed-system")]
pub use server::{ManagementHandler, ManagementServer, ManagementServerOptions};
#[cfg(feature = "managed-system")]
pub use systemd::SystemdManager;
#[cfg(feature = "managed-system")]
pub use user_install::reinstall_user_cli;
#[cfg(feature = "managed-system")]
pub use worker::RedeployWorker;
