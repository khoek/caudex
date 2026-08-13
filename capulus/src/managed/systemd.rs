use std::fs::{self, File};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::time::Duration;

use systemd_zbus::{ActiveState, ManagerProxy, Mode, UnitFileState, UnitProxy};
use zbus::zvariant::Value;

use super::{JobId, ManagedProduct};

#[derive(Clone, Debug)]
pub(super) struct SystemdManager;

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
        let executable = product
            .program()
            .installed_path()
            .to_str()
            .expect("validated managed program path is UTF-8")
            .to_string();
        let arguments = std::iter::once(executable.clone())
            .chain(product.program().command_prefix().iter().cloned())
            .chain([
                "redeploy-worker".to_string(),
                "--job".to_string(),
                job.to_string(),
            ])
            .collect::<Vec<_>>();
        let exec_start = vec![(executable, arguments, false)];
        let runtime_max = duration_microseconds(product.redeploy_runtime_max())?;
        let network_online = vec!["network-online.target"];
        let properties = vec![
            ("Description", Value::from(description.as_str())),
            ("Type", Value::from("exec")),
            ("User", Value::from("root")),
            ("Group", Value::from("root")),
            ("UMask", Value::from(0o077_u32)),
            ("ExecStart", Value::from(exec_start)),
            ("KillMode", Value::from("control-group")),
            ("Restart", Value::from("no")),
            ("OOMPolicy", Value::from("stop")),
            ("RuntimeMaxUSec", Value::from(runtime_max)),
            ("TimeoutStopUSec", Value::from(30_000_000_u64)),
            ("StandardOutput", Value::from("journal")),
            ("StandardError", Value::from("journal")),
            ("SyslogIdentifier", Value::from(product.name())),
            ("NoNewPrivileges", Value::from(true)),
            ("PrivateTmp", Value::from(true)),
            ("CPUAccounting", Value::from(true)),
            ("CPUWeight", Value::from(100_u64)),
            ("MemoryAccounting", Value::from(true)),
            ("IOAccounting", Value::from(true)),
            ("IOWeight", Value::from(100_u64)),
            ("TasksAccounting", Value::from(true)),
            ("TasksMax", Value::from(product.maximum_tasks())),
            ("Wants", Value::from(network_online.clone())),
            ("After", Value::from(network_online)),
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

    pub(super) async fn redeploy_is_active(
        &self,
        product: &ManagedProduct,
        job: JobId,
    ) -> Result<bool, SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        let unit = Self::redeploy_unit_name(product, job);
        let path = match manager.get_unit(&unit).await {
            Ok(path) => path,
            Err(error) if missing_unit_file(&error) => return Ok(false),
            Err(source) => return Err(operation(format!("inspect transient unit {unit}"), source)),
        };
        let proxy = UnitProxy::builder(&connection)
            .path(path)
            .map_err(|source| operation(format!("address transient unit {unit}"), source))?
            .build()
            .await
            .map_err(|source| operation(format!("inspect transient unit {unit}"), source))?;
        proxy
            .active_state()
            .await
            .map(|state| {
                matches!(
                    state,
                    ActiveState::Active
                        | ActiveState::Activating
                        | ActiveState::Reloading
                        | ActiveState::Deactivating
                )
            })
            .map_err(|source| operation(format!("inspect transient unit {unit}"), source))
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
        previously_enabled: &[String],
        previous_service_file: bool,
    ) -> Result<(), SystemdError> {
        let connection = zbus::Connection::system().await.map_err(connect_error)?;
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(connect_error)?;
        let cold_cutover = !previous_service_file;
        if cold_cutover {
            stop_product_runtime(&connection, &manager, product).await?;
            remove_application_socket(product)?;
        } else {
            stop_obsolete_units(
                &connection,
                &manager,
                previously_enabled,
                target_enable_units,
            )
            .await?;
        }
        restore_unit_enablement(&manager, target_enable_units, previously_enabled).await?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload systemd units", source))?;
        for socket in target_enable_units
            .iter()
            .filter(|unit| unit.ends_with(".socket"))
        {
            manager
                .start_unit(socket, Mode::Replace)
                .await
                .map_err(|source| operation(format!("start {socket}"), source))?;
        }
        if cold_cutover {
            manager
                .start_unit(&product.service_name(), Mode::Replace)
                .await
                .map_err(|source| operation(format!("start {}", product.service_name()), source))?;
        } else {
            manager
                .restart_unit(&product.service_name(), Mode::Replace)
                .await
                .map_err(|source| {
                    operation(format!("restart {}", product.service_name()), source)
                })?;
        }
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
        let units = product.installation_manifest().enable_units;
        let previously_enabled = self.enabled_units(product).await?;
        stop_obsolete_units(&connection, &manager, &previously_enabled, &units).await?;
        restore_unit_enablement(&manager, &units, &previously_enabled).await?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload systemd units", source))?;
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
        let cold_cutover = !previous_service_file;
        if cold_cutover {
            stop_product_runtime(&connection, &manager, product).await?;
            remove_application_socket(product)?;
        } else {
            stop_obsolete_units(
                &connection,
                &manager,
                target_enable_units,
                previously_enabled,
            )
            .await?;
        }
        restore_unit_enablement(&manager, previously_enabled, target_enable_units).await?;
        manager
            .reload()
            .await
            .map_err(|source| operation("reload restored systemd units", source))?;
        if previous_service_file {
            for socket in previously_enabled
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

async fn stop_product_runtime(
    connection: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    product: &ManagedProduct,
) -> Result<(), SystemdError> {
    for unit in [
        product.application_socket_name(),
        product.management_socket_name(),
        product.service_name(),
    ] {
        stop_unit_if_present(connection, manager, &unit).await?;
    }
    Ok(())
}

async fn stop_obsolete_units(
    connection: &zbus::Connection,
    manager: &ManagerProxy<'_>,
    previous: &[String],
    target: &[String],
) -> Result<(), SystemdError> {
    for unit in previous.iter().filter(|unit| !target.contains(unit)) {
        stop_unit_if_present(connection, manager, unit).await?;
    }
    Ok(())
}

async fn restore_unit_enablement(
    manager: &ManagerProxy<'_>,
    target: &[String],
    previous: &[String],
) -> Result<(), SystemdError> {
    let disable = previous
        .iter()
        .filter(|unit| !target.contains(unit))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !disable.is_empty() {
        manager
            .disable_unit_files(&disable, false)
            .await
            .map_err(|source| operation("disable obsolete managed units", source))?;
    }
    if !target.is_empty() {
        manager
            .enable_unit_files(
                &target.iter().map(String::as_str).collect::<Vec<_>>(),
                false,
                false,
            )
            .await
            .map_err(|source| operation("enable managed units", source))?;
    }
    Ok(())
}

fn remove_application_socket(product: &ManagedProduct) -> Result<(), SystemdError> {
    let path = product.application_socket_path();
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == 0 => {
            fs::remove_file(path).map_err(|source| SystemdError::SocketRemoval {
                path: path.to_path_buf(),
                source,
            })?;
            File::open(
                path.parent()
                    .expect("validated application socket has a parent"),
            )
            .and_then(|directory| directory.sync_all())
            .map_err(|source| SystemdError::SocketRemoval {
                path: path.to_path_buf(),
                source,
            })
        }
        Ok(_) => Err(SystemdError::UnsafeApplicationSocket(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SystemdError::SocketRemoval {
            path: path.to_path_buf(),
            source,
        }),
    }
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
    matches!(error, zbus::Error::MethodError(name, _, _) if missing_unit_error_name(name.as_str()))
}

fn missing_unit_error_name(name: &str) -> bool {
    matches!(
        name,
        "org.freedesktop.systemd1.NoSuchUnit"
            | "org.freedesktop.systemd1.NoSuchUnitFile"
            | "org.freedesktop.DBus.Error.FileNotFound"
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
    #[error("refusing to remove an unexpected application socket at {0}")]
    UnsafeApplicationSocket(std::path::PathBuf),
    #[error("failed to remove application socket {path}: {source}")]
    SocketRemoval {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use semver::Version;

    use super::*;
    use crate::managed::{
        AgentServiceOptions, ManagedProductOptions, ManagedProgramOptions, ManagedRedeployOptions,
        ServiceHardening, SocketOptions,
    };

    fn product() -> ManagedProduct {
        ManagedProductOptions {
            product: "auc".to_string(),
            package: "auc-tool".to_string(),
            version: Version::new(0, 1, 0),
            program: ManagedProgramOptions {
                cargo_binary: "auc".to_string(),
                installed_path: PathBuf::from("/usr/local/bin/auc"),
                command_prefix: vec!["agent".to_string()],
            },
            service: AgentServiceOptions {
                description: "auc agent".to_string(),
                command: vec!["serve".to_string()],
                restart_delay: Duration::from_secs(5),
                network_required: false,
                state_directory_mode: 0o700,
                hardening: ServiceHardening::Strict {
                    read_write_paths: vec![PathBuf::from("/var/lib/auc")],
                    device_allow: vec![PathBuf::from("/dev/uhid")],
                },
            },
            application_socket: SocketOptions {
                path: PathBuf::from("/run/auc/agent.sock"),
                mode: 0o660,
                group: Some("auc".to_string()),
            },
            management_socket: SocketOptions {
                path: PathBuf::from("/run/auc/capulus.sock"),
                mode: 0o660,
                group: Some("auc".to_string()),
            },
            redeploy: ManagedRedeployOptions {
                build_timeout: Duration::from_secs(600),
                ..ManagedRedeployOptions::default()
            },
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

    #[test]
    fn recognizes_systemd_missing_unit_errors() {
        for name in [
            "org.freedesktop.systemd1.NoSuchUnit",
            "org.freedesktop.systemd1.NoSuchUnitFile",
            "org.freedesktop.DBus.Error.FileNotFound",
        ] {
            assert!(missing_unit_error_name(name));
        }
        assert!(!missing_unit_error_name(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
    }
}
