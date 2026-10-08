use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use super::account::ensure_root_directory;
use super::{JobId, ManagedProduct};

#[derive(Clone, Debug)]
pub(super) struct ServiceManager;

#[derive(Debug, thiserror::Error)]
#[error("launchd operation failed: {0:#}")]
pub(super) struct ServiceError(#[from] anyhow::Error);

impl ServiceManager {
    pub fn redeploy_unit_name(product: &ManagedProduct, job: JobId) -> String {
        format!("{}-redeploy-{job}.plist", product.name())
    }

    pub async fn start_redeploy(
        &self,
        product: &ManagedProduct,
        job: JobId,
    ) -> Result<String, ServiceError> {
        let unit = Self::redeploy_unit_name(product, job);
        let directory = Path::new(super::layout::JOB_RUNTIME).join(product.name());
        ensure_root_directory(&directory, 0o700)?;
        let path = directory.join(&unit);
        let arguments = std::iter::once(product.program().installed_path().display().to_string())
            .chain(product.program().command_prefix().iter().cloned())
            .chain(["redeploy-worker".into(), "--job".into(), job.to_string()])
            .collect::<Vec<_>>();
        let log = Path::new(super::layout::STATE_DIRECTORY)
            .join("jobs")
            .join(product.name())
            .join(format!("{job}.log"));
        let contents = format!(
            "{}<key>RunAtLoad</key><true/><key>KeepAlive</key><false/><key>ProcessType</key><string>Background</string>\
             <key>AbandonProcessGroup</key><false/><key>ExitTimeOut</key><integer>30</integer>\
             <key>StandardOutPath</key>{}<key>StandardErrorPath</key>{}</dict></plist>\n",
            plist_prefix(label(&unit), &arguments),
            xml_string(&log.display().to_string()),
            xml_string(&log.display().to_string()),
        );
        crate::store::atomic_write(&path, contents.as_bytes(), Some(0o600), Some(0o700))?;
        bootstrap(&path).await?;
        Ok(unit)
    }

    pub async fn redeploy_is_active(
        &self,
        product: &ManagedProduct,
        job: JobId,
    ) -> Result<bool, ServiceError> {
        let unit = Self::redeploy_unit_name(product, job);
        let state = inspect(&unit).await?;
        Ok(state.is_some_and(|state| state.lines().any(|line| line.trim() == "state = running")))
    }

    pub async fn finish_redeploy(
        &self,
        product: &ManagedProduct,
        job: JobId,
    ) -> Result<(), ServiceError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.redeploy_is_active(product, job).await? {
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "completed upgrade worker did not exit; launchd job retained for repair"
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let unit = Self::redeploy_unit_name(product, job);
        bootout(&unit).await?;
        let path = Path::new(super::layout::JOB_RUNTIME)
            .join(product.name())
            .join(unit);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(anyhow::Error::from(error).into()),
        }
    }

    pub async fn enabled_units(
        &self,
        product: &ManagedProduct,
    ) -> Result<Vec<String>, ServiceError> {
        let unit = product.service_name();
        Ok(if inspect(&unit).await?.is_some() {
            vec![unit]
        } else {
            Vec::new()
        })
    }

    pub async fn activate_installation(
        &self,
        product: &ManagedProduct,
        target_enable_units: &[String],
        previously_enabled: &[String],
        previous_service_file: bool,
    ) -> Result<(), ServiceError> {
        if target_enable_units != [product.service_name()] {
            return Err(anyhow::anyhow!("invalid launchd service selection").into());
        }
        if previous_service_file || !previously_enabled.is_empty() {
            bootout(&product.service_name()).await?;
        }
        bootstrap(&unit_path(&product.service_name())).await?;
        Ok(())
    }

    pub async fn refresh_installation(&self, product: &ManagedProduct) -> Result<(), ServiceError> {
        if inspect(&product.service_name()).await?.is_none() {
            bootstrap(&unit_path(&product.service_name())).await?;
        }
        Ok(())
    }

    pub async fn restore_installation(
        &self,
        product: &ManagedProduct,
        target_enable_units: &[String],
        previously_enabled: &[String],
        previous_service_file: bool,
    ) -> Result<(), ServiceError> {
        if target_enable_units
            .iter()
            .any(|unit| unit != &product.service_name())
        {
            return Err(anyhow::anyhow!("invalid launchd rollback selection").into());
        }
        bootout(&product.service_name()).await?;
        if previous_service_file && previously_enabled.contains(&product.service_name()) {
            bootstrap(&unit_path(&product.service_name())).await?;
        }
        Ok(())
    }

    pub async fn deactivate_installation(
        &self,
        product: &ManagedProduct,
    ) -> Result<(), ServiceError> {
        bootout(&product.service_name()).await?;
        Ok(())
    }

    pub async fn reload_removed_installation(&self) -> Result<(), ServiceError> {
        // launchd bootstrap domains do not cache unloaded property-list files.
        Ok(())
    }
}

fn unit_path(unit: &str) -> PathBuf {
    Path::new(super::layout::UNIT_DIRECTORY).join(unit)
}

fn label(unit: &str) -> &str {
    unit.strip_suffix(".plist")
        .expect("validated launchd service filename")
}

async fn inspect(unit: &str) -> Result<Option<String>> {
    let output = launchctl(vec!["print".into(), format!("system/{}", label(unit))]).await?;
    if output.status.success() {
        Ok(Some(output.stdout))
    } else if output.status.code() == Some(113) || output.stderr.contains("Could not find service")
    {
        Ok(None)
    } else {
        anyhow::bail!("inspect launchd service {unit}: {}", output.stderr.trim())
    }
}

async fn bootout(unit: &str) -> Result<()> {
    if inspect(unit).await?.is_none() {
        return Ok(());
    }
    successful(vec!["bootout".into(), format!("system/{}", label(unit))]).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while inspect(unit).await?.is_some() {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "launchd service {unit} did not unload before its deadline"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn bootstrap(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    ensure!(
        metadata.file_type().is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
        "launchd property list must be a root-owned file: {}",
        path.display()
    );
    successful(vec![
        "bootstrap".into(),
        "system".into(),
        path.display().to_string(),
    ])
    .await
}

async fn successful(arguments: Vec<String>) -> Result<()> {
    let output = launchctl(arguments).await?;
    ensure!(
        output.status.success(),
        "launchctl failed: {}",
        output.stderr.trim()
    );
    Ok(())
}

async fn launchctl(arguments: Vec<String>) -> Result<crate::process::CommandOutput> {
    tokio::task::spawn_blocking(move || {
        crate::process::CaptureOptions {
            timeout: Duration::from_secs(30),
            ..Default::default()
        }
        .validate()?
        .run(
            Command::new("/bin/launchctl")
                .env_clear()
                .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
                .env("LC_ALL", "C")
                .args(arguments),
            None,
        )
    })
    .await
    .context("launchctl task failed")?
}

pub(super) fn xml_string(value: &str) -> String {
    format!(
        "<string>{}</string>",
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    )
}

pub(super) fn plist_prefix(label: &str, arguments: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
        <key>Label</key>{}<key>ProgramArguments</key><array>{}</array>\
        <key>UserName</key><string>root</string><key>Umask</key><integer>63</integer>",
        xml_string(label),
        arguments
            .iter()
            .map(|argument| xml_string(argument))
            .collect::<String>()
    )
}
