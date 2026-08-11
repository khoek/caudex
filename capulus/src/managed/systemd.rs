use std::time::Duration;

use systemd_zbus::{ActiveState, ManagerProxy, Mode, UnitFileState, UnitProxy};
use zbus::zvariant::Value;

use super::{JobId, ManagedProduct};

#[derive(Clone, Debug)]
pub struct SystemdManager;

impl SystemdManager {
    pub fn redeploy_unit_name(product: &ManagedProduct, job: JobId) -> String {
        format!("{}-redeploy-{job}.service", product.name())
    }

    pub async fn start_redeploy(
        &self,
        product: &ManagedProduct,
        job: JobId,
    ) -> Result<String, SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        let unit = Self::redeploy_unit_name(product, job);
        let description = format!("{} managed redeploy {job}", product.name());
        let executable = product.agent_executable().display().to_string();
        let arguments = vec![
            executable.clone(),
            "redeploy-worker".to_string(),
            "--job".to_string(),
            job.to_string(),
        ];
        let exec_start = vec![(executable, arguments, false)];
        let runtime_max =
            duration_microseconds(product.build_timeout() + Duration::from_secs(300))?;
        let properties = vec![
            ("Description", Value::from(description.as_str())),
            ("Type", Value::from("exec")),
            ("User", Value::from("root")),
            ("Group", Value::from("root")),
            ("UMask", Value::from(0o077_u32)),
            ("ExecStart", Value::from(exec_start)),
            ("KillMode", Value::from("control-group")),
            ("Restart", Value::from("no")),
            ("RuntimeMaxUSec", Value::from(runtime_max)),
            ("TimeoutStopUSec", Value::from(30_000_000_u64)),
            ("StandardOutput", Value::from("journal")),
            ("StandardError", Value::from("journal")),
            ("SyslogIdentifier", Value::from(product.name())),
            ("NoNewPrivileges", Value::from(true)),
            ("PrivateTmp", Value::from(true)),
            ("CPUAccounting", Value::from(true)),
            ("MemoryAccounting", Value::from(true)),
            ("TasksAccounting", Value::from(true)),
            ("CollectMode", Value::from("inactive-or-failed")),
        ];
        manager
            .start_transient_unit(&unit, Mode::Fail, &properties, &[])
            .await
            .map_err(|source| SystemdError::Start {
                unit: unit.clone(),
                source: Box::new(source),
            })?;
        Ok(unit)
    }

    pub(super) async fn enabled_units(
        &self,
        product: &ManagedProduct,
    ) -> Result<Vec<String>, SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        let mut enabled = Vec::new();
        for unit in managed_unit_names(product) {
            match manager.get_unit_file_state(&unit).await {
                Ok(
                    UnitFileState::Enabled
                    | UnitFileState::EnabledRuntime
                    | UnitFileState::Linked
                    | UnitFileState::LinkedRuntime,
                ) => enabled.push(unit),
                Ok(_) => {}
                Err(error) if missing_unit_file(&error) => {}
                Err(source) => {
                    return Err(SystemdError::Operation {
                        action: format!("inspect unit-file state for {unit}"),
                        source: Box::new(source),
                    });
                }
            }
        }
        Ok(enabled)
    }

    pub(super) async fn activate_installation(
        &self,
        product: &ManagedProduct,
        target_enable_units: &[String],
    ) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload systemd units", source))?;
        manager
            .enable_unit_files(
                &target_enable_units
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                false,
                false,
            )
            .await
            .map_err(|source| operation("enable managed units", source))?;
        for socket in target_enable_units
            .iter()
            .filter(|unit| unit.ends_with(".socket"))
        {
            manager
                .start_unit(socket, Mode::Replace)
                .await
                .map_err(|source| operation(format!("start {socket}"), source))?;
        }
        manager
            .restart_unit(&product.service_name(), Mode::Replace)
            .await
            .map_err(|source| operation(format!("restart {}", product.service_name()), source))?;
        Ok(())
    }

    pub(super) async fn refresh_installation(
        &self,
        product: &ManagedProduct,
    ) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload systemd units", source))?;
        let units = product.installation_manifest().enable_units;
        manager
            .enable_unit_files(
                &units.iter().map(String::as_str).collect::<Vec<_>>(),
                false,
                false,
            )
            .await
            .map_err(|source| operation("enable managed units", source))?;
        for socket in units.iter().filter(|unit| unit.ends_with(".socket")) {
            manager
                .start_unit(socket, Mode::Replace)
                .await
                .map_err(|source| operation(format!("start {socket}"), source))?;
        }
        Ok(())
    }

    pub(super) async fn restore_installation(
        &self,
        product: &ManagedProduct,
        target_enable_units: &[String],
        previously_enabled: &[String],
        previous_service_file: bool,
    ) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        let restored_units = product.installation_manifest().enable_units;
        for unit in target_enable_units
            .iter()
            .filter(|unit| !restored_units.contains(unit))
        {
            stop_unit_if_present(&connection, &manager, unit).await?;
        }
        manager
            .reload()
            .await
            .map_err(|source| operation("reload restored systemd units", source))?;
        if !previously_enabled.is_empty() {
            manager
                .enable_unit_files(
                    &previously_enabled
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    false,
                    false,
                )
                .await
                .map_err(|source| operation("restore managed unit enablement", source))?;
        }
        let managed_units = target_enable_units
            .iter()
            .chain(&restored_units)
            .collect::<std::collections::BTreeSet<_>>();
        let newly_enabled = managed_units
            .into_iter()
            .filter(|unit| !previously_enabled.contains(unit))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !newly_enabled.is_empty() {
            manager
                .disable_unit_files(&newly_enabled, false)
                .await
                .map_err(|source| operation("restore managed unit enablement", source))?;
        }
        if previous_service_file {
            for socket in restored_units
                .iter()
                .filter(|unit| unit.ends_with(".socket"))
            {
                manager
                    .start_unit(socket, Mode::Replace)
                    .await
                    .map_err(|source| operation(format!("restore {socket}"), source))?;
            }
            manager
                .restart_unit(&product.service_name(), Mode::Replace)
                .await
                .map_err(|source| {
                    operation(
                        format!("restart restored {}", product.service_name()),
                        source,
                    )
                })?;
        } else {
            for unit in managed_unit_names(product) {
                stop_unit_if_present(&connection, &manager, &unit).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn deactivate_installation(
        &self,
        product: &ManagedProduct,
    ) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        for unit in managed_unit_names(product) {
            stop_unit_if_present(&connection, &manager, &unit).await?;
        }
        let units = product.installation_manifest().enable_units;
        manager
            .disable_unit_files(&units.iter().map(String::as_str).collect::<Vec<_>>(), false)
            .await
            .map_err(|source| operation("disable managed units", source))?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload systemd after deactivation", source))
    }

    pub(super) async fn reload_removed_installation(&self) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?
            .reload()
            .await
            .map_err(|source| operation("reload systemd after managed file removal", source))
    }
}

fn managed_unit_names(product: &ManagedProduct) -> [String; 3] {
    [
        product.service_name(),
        product.application_socket_name(),
        product.management_socket_name(),
    ]
}

async fn stop_unit_if_present(
    connection: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    unit: &str,
) -> Result<(), SystemdError> {
    match manager.stop_unit(unit, Mode::Replace).await {
        Ok(_) => wait_until_inactive(connection, manager, unit).await,
        Err(error) if missing_unit_file(&error) => Ok(()),
        Err(source) => Err(operation(format!("stop {unit}"), source)),
    }
}

async fn wait_until_inactive(
    connection: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    unit: &str,
) -> Result<(), SystemdError> {
    let path = match manager.get_unit(unit).await {
        Ok(path) => path,
        Err(error) if missing_unit_file(&error) => return Ok(()),
        Err(source) => return Err(operation(format!("inspect stopped unit {unit}"), source)),
    };
    let proxy = UnitProxy::builder(connection)
        .path(path)
        .map_err(|source| operation(format!("address stopped unit {unit}"), source))?
        .build()
        .await
        .map_err(|source| operation(format!("inspect stopped unit {unit}"), source))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match proxy.active_state().await {
            Ok(ActiveState::Inactive | ActiveState::Failed) => return Ok(()),
            Ok(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(_) => {
                return Err(SystemdError::UnitStopTimeout(unit.to_string()));
            }
            Err(error) if missing_unit_file(&error) => return Ok(()),
            Err(source) => {
                return Err(operation(format!("inspect stopped unit {unit}"), source));
            }
        }
    }
}

fn operation(action: impl Into<String>, source: zbus::Error) -> SystemdError {
    SystemdError::Operation {
        action: action.into(),
        source: Box::new(source),
    }
}

fn connect_error(source: zbus::Error) -> SystemdError {
    SystemdError::Connect(Box::new(source))
}

fn missing_unit_file(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.systemd1.NoSuchUnit"
                    | "org.freedesktop.systemd1.NoSuchUnitFile"
            )
    )
}

fn duration_microseconds(duration: Duration) -> Result<u64, SystemdError> {
    u64::try_from(duration.as_micros()).map_err(|_| SystemdError::DurationOverflow)
}

#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("failed to connect to the systemd system bus: {0}")]
    Connect(Box<zbus::Error>),
    #[error("failed to start transient unit {unit}: {source}")]
    Start {
        unit: String,
        #[source]
        source: Box<zbus::Error>,
    },
    #[error("failed to {action}: {source}")]
    Operation {
        action: String,
        #[source]
        source: Box<zbus::Error>,
    },
    #[error("managed build timeout cannot be represented for systemd")]
    DurationOverflow,
    #[error("systemd unit {0} did not stop before the deadline")]
    UnitStopTimeout(String),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use semver::Version;

    use super::*;
    use crate::managed::{
        AgentServiceOptions, ApplicationSocketOptions, ManagedProductOptions, ServiceHardening,
        SocketOptions, SystemBinary, UserBinary,
    };

    fn product() -> ManagedProduct {
        ManagedProductOptions {
            product: "auc".to_string(),
            package: "auc-tool".to_string(),
            version: Version::new(0, 1, 0),
            system_binaries: vec![
                SystemBinary {
                    cargo_name: "auc".to_string(),
                    destination: PathBuf::from("/usr/local/bin/auc"),
                },
                SystemBinary {
                    cargo_name: "auc-agent".to_string(),
                    destination: PathBuf::from("/usr/local/bin/auc-agent"),
                },
            ],
            user_binary: UserBinary {
                cargo_name: "auc".to_string(),
            },
            agent_binary: PathBuf::from("/usr/local/bin/auc-agent"),
            service: AgentServiceOptions {
                description: "auc agent".to_string(),
                executable: PathBuf::from("/usr/local/bin/auc-agent"),
                arguments: vec!["serve".to_string()],
                restart_delay: Duration::from_secs(5),
                network_required: false,
                hardening: ServiceHardening::Strict {
                    read_write_paths: vec![PathBuf::from("/var/lib/auc")],
                    device_allow: vec![PathBuf::from("/dev/uhid")],
                },
            },
            application_socket: ApplicationSocketOptions::SystemdActivated(SocketOptions {
                path: PathBuf::from("/run/auc/agent.sock"),
                mode: 0o660,
                group: Some("auc".to_string()),
            }),
            management_socket: SocketOptions {
                path: PathBuf::from("/run/auc/capulus.sock"),
                mode: 0o660,
                group: Some("auc".to_string()),
            },
            build_timeout: Duration::from_secs(600),
        }
        .validate()
        .unwrap()
    }

    #[test]
    fn transient_unit_name_has_only_fixed_product_and_hex_job() {
        let job = JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        assert_eq!(
            SystemdManager::redeploy_unit_name(&product(), job),
            "auc-redeploy-deadbeefdeadbeefdeadbeefdeadbeef.service"
        );
    }
}
