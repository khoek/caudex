#[cfg(target_os = "linux")]
pub(super) const UNIT_DIRECTORY: &str = "/etc/systemd/system";
#[cfg(target_os = "macos")]
pub(super) const UNIT_DIRECTORY: &str = "/Library/LaunchDaemons";

#[cfg(target_os = "linux")]
pub const PROGRAM_DIRECTORY: &str = "/usr/local/bin";
#[cfg(target_os = "macos")]
pub const PROGRAM_DIRECTORY: &str = "/Library/PrivilegedHelperTools";

#[cfg(target_os = "linux")]
pub const RUNTIME_DIRECTORY: &str = "/run";
#[cfg(target_os = "macos")]
pub const RUNTIME_DIRECTORY: &str = "/private/var/run";

#[cfg(target_os = "linux")]
pub(super) const CAPULUS_RUNTIME: &str = "/run/capulus";
#[cfg(target_os = "macos")]
pub(super) const CAPULUS_RUNTIME: &str = "/private/var/run/capulus";

#[cfg(target_os = "linux")]
pub(super) const JOB_RUNTIME: &str = "/run/capulus/jobs";
#[cfg(target_os = "macos")]
pub(super) const JOB_RUNTIME: &str = "/private/var/run/capulus/jobs";

#[cfg(target_os = "linux")]
pub(super) const LOCK_DIRECTORY: &str = "/run/capulus/locks";
#[cfg(target_os = "macos")]
pub(super) const LOCK_DIRECTORY: &str = "/private/var/run/capulus/locks";

#[cfg(target_os = "linux")]
pub(super) const STATE_DIRECTORY: &str = "/var/lib/capulus";
#[cfg(target_os = "macos")]
pub(super) const STATE_DIRECTORY: &str = "/private/var/lib/capulus";

pub(super) fn valid_unit_name(name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        name.ends_with(".service") || name.ends_with(".socket")
    }
    #[cfg(target_os = "macos")]
    {
        name.ends_with(".plist")
    }
}

#[cfg(target_os = "linux")]
pub(super) const INSTALLATION_STATE: &str = "/var/lib/capulus/installations";
#[cfg(target_os = "macos")]
pub(super) const INSTALLATION_STATE: &str = "/private/var/lib/capulus/installations";
#[cfg(target_os = "linux")]
pub(super) const JOB_STATE: &str = "/var/lib/capulus/jobs";
#[cfg(target_os = "macos")]
pub(super) const JOB_STATE: &str = "/private/var/lib/capulus/jobs";

pub(super) fn service_name(product: &str) -> String {
    format!(
        "{product}-agent.{}",
        if cfg!(target_os = "macos") {
            "plist"
        } else {
            "service"
        }
    )
}

pub(super) fn installation_units(product: &str) -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec![service_name(product)]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            service_name(product),
            format!("{product}-agent.socket"),
            format!("{product}-capulus.socket"),
        ]
    }
}
