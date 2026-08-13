use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Subcommand;

use super::worker::RedeployWorker;
use super::{AgentInfo, JobId, ManagedProduct};

#[derive(Debug, Subcommand)]
pub enum AgentLifecycleCommand {
    #[command(hide = true)]
    InstallationManifest,

    #[command(hide = true)]
    RedeployWorker {
        #[arg(long)]
        job: String,
    },
}

impl AgentLifecycleCommand {
    pub fn run<F>(self, product: Arc<ManagedProduct>, application_health: F) -> Result<()>
    where
        F: Fn() -> Result<AgentInfo> + 'static,
    {
        match self {
            Self::InstallationManifest => {
                println!("{}", product.installation_manifest().to_json()?);
                Ok(())
            }
            Self::RedeployWorker { job } => {
                require_root()?;
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name(format!("{}-redeploy", product.name()))
                    .build()?
                    .block_on(
                        RedeployWorker::new(product)?.run(JobId::parse(&job)?, application_health),
                    )
            }
        }
    }
}

fn require_root() -> Result<()> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        bail!("Capulus lifecycle workers must run as root")
    }
}
