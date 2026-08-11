use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};

const INSTALLATION_SCHEMA_MAJOR: u16 = 1;
const MINIMUM_BUILD_TIMEOUT: Duration = Duration::from_secs(60);
const MAXIMUM_BUILD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const REDEPLOY_FIXED_BUDGET: Duration = Duration::from_secs(35 * 60);
const MINIMUM_TASKS: u64 = 16;
const MAXIMUM_TASKS: u64 = 32_768;

#[derive(Clone, Debug)]
pub struct ManagedProductOptions {
    pub product: String,
    pub package: String,
    pub version: Version,
    pub system_binaries: Vec<SystemBinary>,
    pub user_binary: UserBinary,
    pub agent_binary: PathBuf,
    pub service: AgentServiceOptions,
    pub application_socket: ApplicationSocketOptions,
    pub management_socket: SocketOptions,
    pub redeploy: ManagedRedeployOptions,
}

impl ManagedProductOptions {
    pub fn validate(self) -> Result<ManagedProduct, ProductValidationError> {
        validate_identifier("product", &self.product)?;
        validate_identifier("package", &self.package)?;
        if self.system_binaries.is_empty() {
            return Err(ProductValidationError::MissingSystemBinary);
        }
        if !(MINIMUM_BUILD_TIMEOUT..=MAXIMUM_BUILD_TIMEOUT).contains(&self.redeploy.build_timeout) {
            return Err(ProductValidationError::BuildTimeout);
        }
        if !(MINIMUM_TASKS..=MAXIMUM_TASKS).contains(&self.redeploy.maximum_tasks) {
            return Err(ProductValidationError::MaximumTasks);
        }
        let runtime_max = self
            .redeploy
            .build_timeout
            .checked_mul(2)
            .and_then(|duration| duration.checked_add(REDEPLOY_FIXED_BUDGET))
            .ok_or(ProductValidationError::RedeployRuntime)?;
        let expected_binary_directory = Path::new("/usr/local/bin");
        let mut cargo_names = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        for binary in &self.system_binaries {
            validate_identifier("Cargo binary", &binary.cargo_name)?;
            validate_absolute_path(&binary.destination)?;
            if binary.destination.parent() != Some(expected_binary_directory) {
                return Err(ProductValidationError::BinaryDestination(
                    binary.destination.clone(),
                ));
            }
            if binary
                .destination
                .file_name()
                .and_then(|name| name.to_str())
                != Some(binary.cargo_name.as_str())
            {
                return Err(ProductValidationError::BinaryNameMismatch {
                    cargo_name: binary.cargo_name.clone(),
                    destination: binary.destination.clone(),
                });
            }
            if !cargo_names.insert(binary.cargo_name.clone()) {
                return Err(ProductValidationError::DuplicateBinary(
                    binary.cargo_name.clone(),
                ));
            }
            if !destinations.insert(binary.destination.clone()) {
                return Err(ProductValidationError::DuplicateDestination(
                    binary.destination.clone(),
                ));
            }
        }
        validate_identifier("user Cargo binary", &self.user_binary.cargo_name)?;
        if !cargo_names.contains(&self.user_binary.cargo_name) {
            return Err(ProductValidationError::MissingUserBinary(
                self.user_binary.cargo_name,
            ));
        }
        validate_absolute_path(&self.agent_binary)?;
        if !destinations.contains(&self.agent_binary) {
            return Err(ProductValidationError::UnknownAgentBinary(
                self.agent_binary.clone(),
            ));
        }
        validate_service(&self.product, &self.service, &destinations)?;
        validate_socket(
            &self.product,
            "application",
            self.application_socket.options(),
        )?;
        validate_socket(&self.product, "capulus", &self.management_socket)?;
        if self.application_socket.options().path == self.management_socket.path {
            return Err(ProductValidationError::DuplicateSocketPath);
        }
        Ok(ManagedProduct {
            product: self.product,
            package: self.package,
            version: self.version,
            system_binaries: self.system_binaries,
            user_binary: self.user_binary,
            agent_binary: self.agent_binary,
            service: self.service,
            application_socket: self.application_socket,
            management_socket: self.management_socket,
            redeploy: self.redeploy,
            redeploy_runtime_max: runtime_max,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ManagedRedeployOptions {
    pub build_timeout: Duration,
    pub maximum_tasks: u64,
}

impl Default for ManagedRedeployOptions {
    fn default() -> Self {
        Self {
            build_timeout: Duration::from_secs(30 * 60),
            maximum_tasks: 512,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SystemBinary {
    pub cargo_name: String,
    pub destination: PathBuf,
}

#[derive(Clone, Debug)]
pub struct UserBinary {
    pub cargo_name: String,
}

#[derive(Clone, Debug)]
pub struct AgentServiceOptions {
    pub description: String,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub restart_delay: Duration,
    pub network_required: bool,
    pub state_directory_mode: u32,
    pub hardening: ServiceHardening,
}

#[derive(Clone, Debug)]
pub enum ServiceHardening {
    Strict {
        read_write_paths: Vec<PathBuf>,
        device_allow: Vec<PathBuf>,
    },
    SystemNetworkController,
}

#[derive(Clone, Debug)]
pub struct SocketOptions {
    pub path: PathBuf,
    pub mode: u32,
    pub group: Option<String>,
}

#[derive(Clone, Debug)]
pub enum ApplicationSocketOptions {
    AgentBound(SocketOptions),
    SystemdActivated(SocketOptions),
}

impl ApplicationSocketOptions {
    fn options(&self) -> &SocketOptions {
        match self {
            Self::AgentBound(options) | Self::SystemdActivated(options) => options,
        }
    }

    fn is_systemd_activated(&self) -> bool {
        matches!(self, Self::SystemdActivated(_))
    }
}

#[derive(Clone, Debug)]
pub struct ManagedProduct {
    product: String,
    package: String,
    version: Version,
    system_binaries: Vec<SystemBinary>,
    user_binary: UserBinary,
    agent_binary: PathBuf,
    service: AgentServiceOptions,
    application_socket: ApplicationSocketOptions,
    management_socket: SocketOptions,
    redeploy: ManagedRedeployOptions,
    redeploy_runtime_max: Duration,
}

impl ManagedProduct {
    pub fn name(&self) -> &str {
        &self.product
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn system_binaries(&self) -> &[SystemBinary] {
        &self.system_binaries
    }

    pub fn user_binary(&self) -> &UserBinary {
        &self.user_binary
    }

    pub fn build_timeout(&self) -> Duration {
        self.redeploy.build_timeout
    }

    pub fn maximum_tasks(&self) -> u64 {
        self.redeploy.maximum_tasks
    }

    pub fn redeploy_runtime_max(&self) -> Duration {
        self.redeploy_runtime_max
    }

    pub fn application_socket_path(&self) -> &Path {
        &self.application_socket.options().path
    }

    pub fn application_socket_is_systemd_activated(&self) -> bool {
        self.application_socket.is_systemd_activated()
    }

    pub fn management_socket_path(&self) -> &Path {
        &self.management_socket.path
    }

    pub fn agent_executable(&self) -> &Path {
        &self.agent_binary
    }

    pub fn service_name(&self) -> String {
        format!("{}-agent.service", self.product)
    }

    pub fn application_socket_name(&self) -> String {
        format!("{}-agent.socket", self.product)
    }

    pub fn management_socket_name(&self) -> String {
        format!("{}-capulus.socket", self.product)
    }

    pub fn installation_manifest(&self) -> InstallationManifest {
        let service_name = self.service_name();
        let application_socket_name = self.application_socket_name();
        let management_socket_name = self.management_socket_name();
        let mut files = self
            .system_binaries
            .iter()
            .map(|binary| ManagedFile::Binary {
                source_name: binary.cargo_name.clone(),
                destination: binary.destination.clone(),
                mode: 0o755,
            })
            .chain([
                ManagedFile::Text {
                    destination: system_unit_path(&service_name),
                    contents: self.service_unit_contents(),
                    mode: 0o644,
                },
                ManagedFile::Text {
                    destination: system_unit_path(&management_socket_name),
                    contents: self.socket_unit_contents("capulus", &self.management_socket),
                    mode: 0o644,
                },
            ])
            .collect::<Vec<_>>();
        let mut enable_units = vec![management_socket_name, service_name];
        if let ApplicationSocketOptions::SystemdActivated(application_socket) =
            &self.application_socket
        {
            files.push(ManagedFile::Text {
                destination: system_unit_path(&application_socket_name),
                contents: self.socket_unit_contents("application", application_socket),
                mode: 0o644,
            });
            enable_units.insert(0, application_socket_name);
        }
        InstallationManifest {
            schema_major: INSTALLATION_SCHEMA_MAJOR,
            product: self.product.clone(),
            package: self.package.clone(),
            version: self.version.clone(),
            files,
            enable_units,
        }
    }

    pub(crate) fn validate_release_manifest(
        &self,
        manifest: &InstallationManifest,
        expected_version: &Version,
    ) -> Result<(), ProductValidationError> {
        manifest.validate(self.name())?;
        if manifest.package != self.package {
            return Err(ProductValidationError::ManifestPackage {
                actual: manifest.package.clone(),
                expected: self.package.clone(),
            });
        }
        if &manifest.version != expected_version {
            return Err(ProductValidationError::ManifestVersion {
                actual: manifest.version.clone(),
                expected: expected_version.clone(),
            });
        }

        let expected_binaries = self
            .system_binaries
            .iter()
            .map(|binary| (binary.cargo_name.clone(), binary.destination.clone(), 0o755))
            .collect::<BTreeSet<_>>();
        let actual_binaries = manifest
            .files
            .iter()
            .filter_map(|file| match file {
                ManagedFile::Binary {
                    source_name,
                    destination,
                    mode,
                } => Some((source_name.clone(), destination.clone(), *mode)),
                ManagedFile::Text { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        if actual_binaries != expected_binaries {
            return Err(ProductValidationError::ManifestBinarySet);
        }

        let service = self.service_name();
        let management = self.management_socket_name();
        let application = self.application_socket_name();
        let allowed_units = BTreeSet::from([service.clone(), management.clone(), application]);
        let present_units = manifest
            .files
            .iter()
            .filter_map(|file| match file {
                ManagedFile::Text { destination, .. } => destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
                ManagedFile::Binary { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        if !present_units.is_subset(&allowed_units) {
            return Err(ProductValidationError::UnexpectedManagedUnit);
        }
        if !present_units.contains(&service) || !present_units.contains(&management) {
            return Err(ProductValidationError::MissingRequiredUnit);
        }
        if manifest
            .enable_units
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            != present_units
            || manifest.enable_units.len() != present_units.len()
        {
            return Err(ProductValidationError::UnitEnablement);
        }
        Ok(())
    }

    fn service_unit_contents(&self) -> String {
        let management_socket = self.management_socket_name();
        let socket_dependencies = if self.application_socket.is_systemd_activated() {
            let application_socket = self.application_socket_name();
            format!(
                "Requires={application_socket} {management_socket}\n\
                 After={application_socket} {management_socket}\n"
            )
        } else {
            format!("Requires={management_socket}\nAfter={management_socket}\n")
        };
        let network = if self.service.network_required {
            "After=network-online.target\nWants=network-online.target\n"
        } else {
            ""
        };
        let hardening = match &self.service.hardening {
            ServiceHardening::Strict {
                read_write_paths,
                device_allow,
            } => {
                let mut read_write_paths = read_write_paths.clone();
                let capulus_state = PathBuf::from("/var/lib/capulus");
                if !read_write_paths.contains(&capulus_state) {
                    read_write_paths.push(capulus_state);
                }
                let read_write_paths = render_path_list("ReadWritePaths", &read_write_paths);
                let device_allow = device_allow
                    .iter()
                    .map(|path| format!("DeviceAllow={} rw\n", quote_systemd_path(path)))
                    .collect::<String>();
                format!(
                    "NoNewPrivileges=yes\n\
                     PrivateTmp=yes\n\
                     ProtectClock=yes\n\
                     ProtectControlGroups=yes\n\
                     ProtectHome=read-only\n\
                     ProtectKernelLogs=yes\n\
                     ProtectKernelModules=yes\n\
                     ProtectKernelTunables=yes\n\
                     ProtectSystem=strict\n\
                     DevicePolicy=closed\n\
                     RestrictRealtime=yes\n\
                     RestrictSUIDSGID=yes\n\
                     LockPersonality=yes\n\
                     MemoryDenyWriteExecute=yes\n\
                     LimitCORE=0\n\
                     LimitMEMLOCK=65536\n\
                     {read_write_paths}{device_allow}"
                )
            }
            ServiceHardening::SystemNetworkController => concat!(
                "PrivateTmp=yes\n",
                "ProtectClock=yes\n",
                "ProtectHome=read-only\n",
                "ProtectKernelLogs=yes\n",
                "ProtectKernelModules=yes\n",
                "RestrictRealtime=yes\n",
                "LockPersonality=yes\n",
            )
            .to_string(),
        };
        let arguments = self
            .service
            .arguments
            .iter()
            .map(|argument| format!(" {}", quote_systemd_word(argument)))
            .collect::<String>();
        format!(
            "[Unit]\n\
             Description={}\n\
             {socket_dependencies}\
             {network}\
             StartLimitIntervalSec=0\n\
             \n\
             [Service]\n\
             Type=exec\n\
             User=root\n\
             Group=root\n\
             UMask=0077\n\
             ExecStart={}{}\n\
             Restart=always\n\
             RestartSec={}s\n\
             RuntimeDirectory={} capulus\n\
             RuntimeDirectoryMode=0755\n\
             RuntimeDirectoryPreserve=yes\n\
             StateDirectory={}\n\
             StateDirectoryMode={:04o}\n\
             {hardening}\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n",
            self.service.description,
            quote_systemd_path(&self.service.executable),
            arguments,
            self.service.restart_delay.as_secs(),
            self.product,
            self.product,
            self.service.state_directory_mode,
        )
    }

    fn socket_unit_contents(&self, descriptor_name: &str, options: &SocketOptions) -> String {
        let group = options
            .group
            .as_ref()
            .map(|group| format!("SocketGroup={group}\n"))
            .unwrap_or_default();
        format!(
            "[Unit]\n\
             Description={} {descriptor_name} socket\n\
             \n\
             [Socket]\n\
             ListenStream={}\n\
             FileDescriptorName={descriptor_name}\n\
             Service={}\n\
             Accept=no\n\
             SocketMode={:04o}\n\
             {group}\
             DirectoryMode=0755\n\
             RemoveOnStop=yes\n\
             \n\
             [Install]\n\
             WantedBy=sockets.target\n",
            self.product,
            options.path.display(),
            self.service_name(),
            options.mode,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationManifest {
    pub schema_major: u16,
    pub product: String,
    pub package: String,
    pub version: Version,
    pub files: Vec<ManagedFile>,
    pub enable_units: Vec<String>,
}

impl InstallationManifest {
    pub fn validate(&self, expected_product: &str) -> Result<(), ProductValidationError> {
        if self.schema_major != INSTALLATION_SCHEMA_MAJOR {
            return Err(ProductValidationError::InstallationSchema(
                self.schema_major,
            ));
        }
        if self.product != expected_product {
            return Err(ProductValidationError::ManifestProduct {
                actual: self.product.clone(),
                expected: expected_product.to_string(),
            });
        }
        validate_identifier("manifest product", &self.product)?;
        validate_identifier("manifest package", &self.package)?;
        let unit_prefix = format!("{}-", self.product);
        let mut destinations = BTreeSet::new();
        let mut binary_count = 0;
        for file in &self.files {
            let (destination, mode) = match file {
                ManagedFile::Binary {
                    source_name,
                    destination,
                    mode,
                } => {
                    binary_count += 1;
                    validate_identifier("manifest binary", source_name)?;
                    if destination.parent() != Some(Path::new("/usr/local/bin"))
                        || destination.file_name().and_then(|name| name.to_str())
                            != Some(source_name)
                    {
                        return Err(ProductValidationError::BinaryDestination(
                            destination.clone(),
                        ));
                    }
                    (destination, *mode)
                }
                ManagedFile::Text {
                    destination,
                    contents,
                    mode,
                } => {
                    let Some(name) = destination.file_name().and_then(|name| name.to_str()) else {
                        return Err(ProductValidationError::UnitDestination(destination.clone()));
                    };
                    if destination.parent() != Some(Path::new("/etc/systemd/system"))
                        || !name.starts_with(&unit_prefix)
                        || !(name.ends_with(".service") || name.ends_with(".socket"))
                    {
                        return Err(ProductValidationError::UnitDestination(destination.clone()));
                    }
                    if contents.len() > 128 * 1024 || contents.contains('\0') {
                        return Err(ProductValidationError::UnitContents(name.to_string()));
                    }
                    (destination, *mode)
                }
            };
            validate_absolute_path(destination)?;
            if !matches!(mode, 0o644 | 0o755) {
                return Err(ProductValidationError::FileMode(mode));
            }
            if !destinations.insert(destination.clone()) {
                return Err(ProductValidationError::DuplicateDestination(
                    destination.clone(),
                ));
            }
        }
        if binary_count == 0 {
            return Err(ProductValidationError::MissingSystemBinary);
        }
        for unit in &self.enable_units {
            if !unit.starts_with(&unit_prefix)
                || !(unit.ends_with(".service") || unit.ends_with(".socket"))
            {
                return Err(ProductValidationError::UnitName(unit.clone()));
            }
            if !destinations.contains(&system_unit_path(unit)) {
                return Err(ProductValidationError::MissingUnitFile(unit.clone()));
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(value: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", deny_unknown_fields)]
pub enum ManagedFile {
    Binary {
        source_name: String,
        destination: PathBuf,
        mode: u32,
    },
    Text {
        destination: PathBuf,
        contents: String,
        mode: u32,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProductValidationError {
    #[error("{field} must be a lowercase ASCII identifier: {value:?}")]
    Identifier { field: &'static str, value: String },
    #[error("managed product must install at least one system binary")]
    MissingSystemBinary,
    #[error("managed build timeout must be at least 60 seconds")]
    BuildTimeout,
    #[error("managed redeploy task limit must be between 16 and 32768")]
    MaximumTasks,
    #[error("managed redeploy runtime limit cannot be represented")]
    RedeployRuntime,
    #[error("managed binary destination must be directly beneath /usr/local/bin: {0}")]
    BinaryDestination(PathBuf),
    #[error("Cargo binary {cargo_name:?} does not match destination {destination}")]
    BinaryNameMismatch {
        cargo_name: String,
        destination: PathBuf,
    },
    #[error("managed product contains duplicate Cargo binary {0:?}")]
    DuplicateBinary(String),
    #[error("managed product contains duplicate destination {0}")]
    DuplicateDestination(PathBuf),
    #[error("user Cargo binary {0:?} is not among the managed system binaries")]
    MissingUserBinary(String),
    #[error("agent executable {0} is not among the managed system binary destinations")]
    UnknownAgentExecutable(PathBuf),
    #[error("agent binary {0} is not among the managed system binary destinations")]
    UnknownAgentBinary(PathBuf),
    #[error("agent service description or argument contains forbidden control/specifier bytes")]
    UnsafeServiceText,
    #[error("agent restart delay must be between one second and one hour")]
    RestartDelay,
    #[error("agent state-directory mode must grant owner rwx and no permissions outside 0777")]
    StateDirectoryMode,
    #[error("managed path must be absolute, normalized, and contain no parent traversal: {0}")]
    UnsafePath(PathBuf),
    #[error("strict hardening path does not exist beneath an allowed system root: {0}")]
    HardeningPath(PathBuf),
    #[error("socket path must be {expected}, not {actual}")]
    SocketPath { expected: PathBuf, actual: PathBuf },
    #[error("socket mode must grant no permissions outside 0777")]
    SocketMode,
    #[error("application and Capulus sockets must have distinct paths")]
    DuplicateSocketPath,
    #[error("installation manifest schema v{0} is unsupported")]
    InstallationSchema(u16),
    #[error("installation manifest is for {actual:?}, expected {expected:?}")]
    ManifestProduct { actual: String, expected: String },
    #[error("installation manifest package is {actual:?}, expected {expected:?}")]
    ManifestPackage { actual: String, expected: String },
    #[error("installation manifest version is {actual}, expected {expected}")]
    ManifestVersion { actual: Version, expected: Version },
    #[error("installation manifest has a different managed binary set")]
    ManifestBinarySet,
    #[error("installation manifest contains an unexpected managed unit")]
    UnexpectedManagedUnit,
    #[error("installation manifest omits its service or Capulus socket")]
    MissingRequiredUnit,
    #[error("installation manifest unit files and enabled units differ")]
    UnitEnablement,
    #[error("managed unit destination is outside the product namespace: {0}")]
    UnitDestination(PathBuf),
    #[error("managed unit {0:?} has invalid or oversized contents")]
    UnitContents(String),
    #[error("managed file mode {0:o} is not permitted")]
    FileMode(u32),
    #[error("managed unit name is outside the product namespace: {0:?}")]
    UnitName(String),
    #[error("enabled unit has no corresponding managed file: {0:?}")]
    MissingUnitFile(String),
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), ProductValidationError> {
    if !is_valid_identifier(value) {
        return Err(ProductValidationError::Identifier {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

pub(super) fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_lowercase()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn validate_service(
    product: &str,
    service: &AgentServiceOptions,
    binary_destinations: &BTreeSet<PathBuf>,
) -> Result<(), ProductValidationError> {
    if !binary_destinations.contains(&service.executable) {
        return Err(ProductValidationError::UnknownAgentExecutable(
            service.executable.clone(),
        ));
    }
    if service.description.is_empty()
        || unsafe_unit_text(&service.description)
        || service
            .arguments
            .iter()
            .any(|argument| unsafe_unit_text(argument))
    {
        return Err(ProductValidationError::UnsafeServiceText);
    }
    if !(Duration::from_secs(1)..=Duration::from_secs(60 * 60)).contains(&service.restart_delay) {
        return Err(ProductValidationError::RestartDelay);
    }
    if service.restart_delay.subsec_nanos() != 0 {
        return Err(ProductValidationError::RestartDelay);
    }
    if service.state_directory_mode > 0o777 || service.state_directory_mode & 0o700 != 0o700 {
        return Err(ProductValidationError::StateDirectoryMode);
    }
    if let ServiceHardening::Strict {
        read_write_paths,
        device_allow,
    } = &service.hardening
    {
        for path in read_write_paths.iter().chain(device_allow) {
            validate_absolute_path(path)?;
            validate_systemd_path(path)?;
            if !path.starts_with(format!("/var/lib/{product}"))
                && !path.starts_with(format!("/run/{product}"))
                && !path.starts_with("/dev/")
            {
                return Err(ProductValidationError::HardeningPath(path.clone()));
            }
        }
    }
    Ok(())
}

fn validate_socket(
    product: &str,
    descriptor: &str,
    socket: &SocketOptions,
) -> Result<(), ProductValidationError> {
    validate_absolute_path(&socket.path)?;
    let filename = if descriptor == "application" {
        "agent.sock"
    } else {
        "capulus.sock"
    };
    let expected = Path::new("/run").join(product).join(filename);
    if socket.path != expected {
        return Err(ProductValidationError::SocketPath {
            expected,
            actual: socket.path.clone(),
        });
    }
    if socket.mode > 0o777 {
        return Err(ProductValidationError::SocketMode);
    }
    if let Some(group) = &socket.group {
        validate_identifier("socket group", group)?;
    }
    Ok(())
}

fn validate_absolute_path(path: &Path) -> Result<(), ProductValidationError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(ProductValidationError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn unsafe_unit_text(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b'%')
}

fn quote_systemd_path(path: &Path) -> String {
    quote_systemd_word(path.to_str().expect("validated systemd path is UTF-8"))
}

fn validate_systemd_path(path: &Path) -> Result<(), ProductValidationError> {
    let Some(value) = path.to_str() else {
        return Err(ProductValidationError::UnsafePath(path.to_path_buf()));
    };
    if unsafe_unit_text(value) {
        return Err(ProductValidationError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

fn quote_systemd_word(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn render_path_list(name: &str, paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        String::new()
    } else {
        format!(
            "{name}={}\n",
            paths
                .iter()
                .map(|path| quote_systemd_path(path))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }
}

fn system_unit_path(unit: &str) -> PathBuf {
    Path::new("/etc/systemd/system").join(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ManagedProductOptions {
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
                description: "auc virtual authenticator".to_string(),
                executable: PathBuf::from("/usr/local/bin/auc-agent"),
                arguments: vec!["serve".to_string()],
                restart_delay: Duration::from_secs(5),
                network_required: false,
                state_directory_mode: 0o700,
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
            redeploy: ManagedRedeployOptions {
                build_timeout: Duration::from_secs(20 * 60),
                ..ManagedRedeployOptions::default()
            },
        }
    }

    #[test]
    fn validated_product_renders_two_named_sockets_and_service_dependencies() {
        let product = options().validate().unwrap();
        let manifest = product.installation_manifest();
        manifest.validate("auc").unwrap();
        let text = manifest
            .files
            .iter()
            .filter_map(|file| match file {
                ManagedFile::Text { contents, .. } => Some(contents.as_str()),
                ManagedFile::Binary { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("FileDescriptorName=application"));
        assert!(text.contains("FileDescriptorName=capulus"));
        assert!(text.contains("ListenStream=/run/auc/agent.sock"));
        assert!(!text.contains("ListenStream=\""));
        assert!(text.contains("Requires=auc-agent.socket auc-capulus.socket"));
        assert!(text.contains("ProtectHome=read-only"));
        assert!(text.contains("RuntimeDirectory=auc capulus"));
        assert!(text.contains("RuntimeDirectoryPreserve=yes"));
        assert!(text.contains("StateDirectory=auc\n"));
        assert!(text.contains("ReadWritePaths=\"/var/lib/auc\" \"/var/lib/capulus\""));
        assert!(text.contains("StateDirectoryMode=0700"));
        assert!(text.contains("DeviceAllow=\"/dev/uhid\" rw"));
    }

    #[test]
    fn release_manifest_can_move_an_application_socket_to_systemd() {
        let target = options().validate().unwrap();
        let mut bridge_options = options();
        bridge_options.application_socket = ApplicationSocketOptions::AgentBound(SocketOptions {
            path: PathBuf::from("/run/auc/agent.sock"),
            mode: 0o660,
            group: Some("auc".to_string()),
        });
        let bridge = bridge_options.validate().unwrap();
        let bridge_manifest = bridge.installation_manifest();
        let target_manifest = target.installation_manifest();

        assert!(
            !bridge_manifest
                .enable_units
                .contains(&bridge.application_socket_name())
        );
        assert!(
            bridge_manifest
                .files
                .iter()
                .filter_map(|file| match file {
                    ManagedFile::Text { contents, .. } => Some(contents),
                    ManagedFile::Binary { .. } => None,
                })
                .any(|contents| contents.contains("Requires=auc-capulus.socket\n"))
        );
        bridge
            .validate_release_manifest(&target_manifest, target.version())
            .unwrap();
    }

    #[test]
    fn manifest_rejects_cross_product_unit_paths() {
        let product = options().validate().unwrap();
        let mut manifest = product.installation_manifest();
        let ManagedFile::Text { destination, .. } = &mut manifest.files[2] else {
            panic!("expected unit file");
        };
        *destination = PathBuf::from("/etc/systemd/system/other.service");

        assert!(manifest.validate("auc").is_err());
    }

    #[test]
    fn redeploy_limits_and_whole_second_restart_delay_are_validated() {
        let mut invalid = options();
        invalid.redeploy.maximum_tasks = 0;
        assert!(matches!(
            invalid.validate(),
            Err(ProductValidationError::MaximumTasks)
        ));

        let mut invalid = options();
        invalid.service.restart_delay = Duration::from_millis(1500);
        assert!(matches!(
            invalid.validate(),
            Err(ProductValidationError::RestartDelay)
        ));

        let product = options().validate().unwrap();
        assert_eq!(
            product.redeploy_runtime_max(),
            product.build_timeout() * 2 + REDEPLOY_FIXED_BUDGET
        );
    }

    #[test]
    fn options_reject_parent_traversal_and_mismatched_binary_names() {
        let mut unsafe_options = options();
        unsafe_options.system_binaries[0].destination = PathBuf::from("/usr/local/bin/../bin/auc");
        assert!(unsafe_options.validate().is_err());

        let mut mismatch = options();
        mismatch.system_binaries[0].destination = PathBuf::from("/usr/local/bin/not-auc");
        assert!(mismatch.validate().is_err());

        let mut unsafe_state_mode = options();
        unsafe_state_mode.service.state_directory_mode = 0o1700;
        assert!(matches!(
            unsafe_state_mode.validate(),
            Err(ProductValidationError::StateDirectoryMode)
        ));
    }
}
