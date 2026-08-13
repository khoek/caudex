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
mod lifecycle;
#[cfg(feature = "managed-system")]
mod product;
mod protocol;
#[cfg(feature = "managed-system")]
mod release;
#[cfg(feature = "managed-system")]
mod server;
#[cfg(feature = "managed-system")]
mod systemd;
#[cfg(feature = "managed-client")]
mod user_program;
mod validation;
#[cfg(feature = "managed-system")]
mod worker;

#[cfg(feature = "managed-system")]
use account::BuildAccount;
#[cfg(feature = "managed-system")]
pub use account::UnixAccount;
#[cfg(feature = "managed-system")]
pub use activation::{ActivatedListeners, ActivationError};
#[cfg(feature = "managed-system")]
pub use agent::{ManagedAgent, ReleaseSource};
#[cfg(feature = "managed-system")]
pub use build::BuildArtifacts;
#[cfg(feature = "managed-system")]
use build::ManagedBuild;
pub use client::{ManagementClient, ManagementClientOptions};
#[cfg(feature = "managed-system")]
pub use installation::{SystemInstallation, SystemUninstallation};
#[cfg(feature = "managed-system")]
pub use job::{RedeployCoordinator, RedeployRequest};
#[cfg(feature = "managed-system")]
pub use lifecycle::AgentLifecycleCommand;
#[cfg(feature = "managed-system")]
pub use product::{
    AgentServiceOptions, InstallationManifest, ManagedFile, ManagedProduct, ManagedProductOptions,
    ManagedProgram, ManagedProgramOptions, ManagedRedeployOptions, ProductValidationError,
    ServiceHardening, SocketOptions,
};
pub use protocol::{
    AgentInfo, ErrorCode, JobId, JobPhase, ManagementError, ManagementRequest, ManagementResponse,
    PROTOCOL_MAJOR, PeerCredentials, ProtocolError, RedeployJob, RedeployOutcome, RepairOutcome,
    RequestId, ResolvedReleaseInfo, VersionTarget,
};
#[cfg(feature = "managed-system")]
pub use release::{CargoRegistry, ResolvedRelease};
#[cfg(feature = "managed-system")]
pub use server::{ManagementHandler, ManagementServer, ManagementServerOptions};
#[cfg(feature = "managed-system")]
use systemd::SystemdManager;
#[cfg(feature = "managed-client")]
pub use user_program::{UserProgramUpdate, UserProgramUpdateOptions, current_user_cargo_root};
