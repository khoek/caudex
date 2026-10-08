use std::collections::BTreeMap;
#[cfg(any(target_os = "linux", test))]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::env;
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{BorrowedFd, RawFd};
use std::os::unix::net::UnixListener;

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{SocketType, sockopt::socket_type};

#[cfg(target_os = "linux")]
const SYSTEMD_LISTEN_FD_START: RawFd = 3;
#[cfg(target_os = "linux")]
const MAX_ACTIVATED_LISTENERS: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum ActivationError {
    #[error("systemd socket activation environment is incomplete: missing {0}")]
    MissingEnvironment(&'static str),
    #[error("invalid systemd socket activation value for {name}: {value:?}")]
    InvalidEnvironment { name: &'static str, value: String },
    #[error("systemd socket activation belongs to PID {actual}, not this process ({expected})")]
    WrongProcess { actual: u32, expected: u32 },
    #[error("systemd passed {0} listeners, exceeding the safety limit")]
    TooManyListeners(usize),
    #[error("systemd socket descriptor names do not match LISTEN_FDS")]
    NameCountMismatch,
    #[error("service manager passed duplicate socket descriptor name {0:?}")]
    DuplicateName(String),
    #[error("service manager did not pass required socket descriptor {0:?}")]
    MissingName(String),
    #[error("systemd passed unexpected socket descriptor {0:?}")]
    UnexpectedName(String),
    #[error("activated descriptor {name:?} is not a Unix stream listener")]
    WrongSocketType { name: String },
    #[error("failed to adopt activated descriptor {name:?}: {source}")]
    Adopt {
        name: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct ActivatedListeners {
    listeners: BTreeMap<String, UnixListener>,
}

impl ActivatedListeners {
    /// Adopts named service-manager descriptors once during single-threaded process startup.
    #[cfg(target_os = "linux")]
    pub fn from_environment(required_names: &[&str]) -> Result<Self, ActivationError> {
        let listen_pid = required_env("LISTEN_PID")?;
        let actual = parse_u32("LISTEN_PID", &listen_pid)?;
        let expected = std::process::id();
        if actual != expected {
            return Err(ActivationError::WrongProcess { actual, expected });
        }
        let listen_fds = required_env("LISTEN_FDS")?;
        let count = parse_usize("LISTEN_FDS", &listen_fds)?;
        if count > MAX_ACTIVATED_LISTENERS {
            return Err(ActivationError::TooManyListeners(count));
        }
        let names = required_env("LISTEN_FDNAMES")?;
        let names = parse_names(count, &names, required_names)?;
        let mut listeners = BTreeMap::new();
        for (index, name) in names.into_iter().enumerate() {
            let raw_fd = SYSTEMD_LISTEN_FD_START + index as RawFd;
            // SAFETY: systemd promises ownership of consecutive descriptors starting at 3 to the
            // process named by LISTEN_PID. We validated the PID, count, and unique descriptor name.
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            if socket_type(borrowed).map_err(|source| ActivationError::Adopt {
                name: name.clone(),
                source: source.into(),
            })? != SocketType::STREAM
            {
                return Err(ActivationError::WrongSocketType { name });
            }
            fcntl_setfd(borrowed, FdFlags::CLOEXEC).map_err(|source| ActivationError::Adopt {
                name: name.clone(),
                source: source.into(),
            })?;
            // SAFETY: this is the sole ownership transfer for each validated systemd descriptor.
            let listener = unsafe { UnixListener::from_raw_fd(raw_fd) };
            listener
                .local_addr()
                .map_err(|source| ActivationError::Adopt {
                    name: name.clone(),
                    source,
                })?;
            listeners.insert(name, listener);
        }
        clear_activation_environment();
        Ok(Self { listeners })
    }

    /// Adopts launchd's named descriptors once during single-threaded process startup.
    #[cfg(target_os = "macos")]
    pub fn from_environment(required_names: &[&str]) -> Result<Self, ActivationError> {
        use std::ffi::CString;
        use std::os::fd::OwnedFd;
        unsafe extern "C" {
            fn launch_activate_socket(
                name: *const libc::c_char,
                descriptors: *mut *mut libc::c_int,
                count: *mut libc::size_t,
            ) -> libc::c_int;
        }
        let mut listeners = BTreeMap::new();
        for &name in required_names {
            if listeners.contains_key(name) {
                return Err(ActivationError::DuplicateName(name.into()));
            }
            let c_name =
                CString::new(name).map_err(|_| ActivationError::MissingName(name.into()))?;
            let mut descriptors = std::ptr::null_mut();
            let mut count = 0;
            // SAFETY: launchd allocates an array of owned descriptors; we free the array and adopt every fd.
            let status =
                unsafe { launch_activate_socket(c_name.as_ptr(), &mut descriptors, &mut count) };
            if status != 0 {
                return Err(ActivationError::Adopt {
                    name: name.into(),
                    source: std::io::Error::from_raw_os_error(status),
                });
            }
            if descriptors.is_null() {
                return Err(ActivationError::MissingName(name.into()));
            }
            let mut owned = unsafe { std::slice::from_raw_parts(descriptors, count) }
                .iter()
                .map(|fd| unsafe { OwnedFd::from_raw_fd(*fd) })
                .collect::<Vec<_>>();
            unsafe {
                libc::free(descriptors.cast());
            }
            if owned.len() != 1 {
                return Err(ActivationError::WrongSocketType { name: name.into() });
            }
            let descriptor = owned.pop().expect("one activated listener");
            if socket_type(&descriptor).map_err(|source| ActivationError::Adopt {
                name: name.into(),
                source: source.into(),
            })? != SocketType::STREAM
            {
                return Err(ActivationError::WrongSocketType { name: name.into() });
            }
            fcntl_setfd(&descriptor, FdFlags::CLOEXEC).map_err(|source| {
                ActivationError::Adopt {
                    name: name.into(),
                    source: source.into(),
                }
            })?;
            let listener = UnixListener::from(descriptor);
            listener
                .local_addr()
                .map_err(|source| ActivationError::Adopt {
                    name: name.into(),
                    source,
                })?;
            listeners.insert(name.into(), listener);
        }
        Ok(Self { listeners })
    }

    pub fn take(&mut self, name: &str) -> Result<UnixListener, ActivationError> {
        self.listeners
            .remove(name)
            .ok_or_else(|| ActivationError::MissingName(name.to_string()))
    }

    pub fn take_tokio(&mut self, name: &str) -> Result<tokio::net::UnixListener, ActivationError> {
        let listener = self.take(name)?;
        listener
            .set_nonblocking(true)
            .map_err(|source| ActivationError::Adopt {
                name: name.to_string(),
                source,
            })?;
        tokio::net::UnixListener::from_std(listener).map_err(|source| ActivationError::Adopt {
            name: name.to_string(),
            source,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.listeners.is_empty()
    }
}

#[cfg(target_os = "linux")]
fn required_env(name: &'static str) -> Result<String, ActivationError> {
    env::var(name).map_err(|_| ActivationError::MissingEnvironment(name))
}

#[cfg(target_os = "linux")]
fn parse_u32(name: &'static str, value: &str) -> Result<u32, ActivationError> {
    value
        .parse()
        .map_err(|_| ActivationError::InvalidEnvironment {
            name,
            value: value.to_string(),
        })
}

#[cfg(target_os = "linux")]
fn parse_usize(name: &'static str, value: &str) -> Result<usize, ActivationError> {
    value
        .parse()
        .map_err(|_| ActivationError::InvalidEnvironment {
            name,
            value: value.to_string(),
        })
}

#[cfg(any(target_os = "linux", test))]
fn parse_names(
    count: usize,
    value: &str,
    required_names: &[&str],
) -> Result<Vec<String>, ActivationError> {
    let names = if count == 0 && value.is_empty() {
        Vec::new()
    } else {
        value.split(':').map(str::to_owned).collect::<Vec<_>>()
    };
    if names.len() != count || names.iter().any(String::is_empty) {
        return Err(ActivationError::NameCountMismatch);
    }
    let unique = names.iter().cloned().collect::<BTreeSet<_>>();
    if unique.len() != names.len() {
        return Err(ActivationError::DuplicateName(
            names
                .iter()
                .find(|name| names.iter().filter(|candidate| *candidate == *name).count() > 1)
                .cloned()
                .expect("a duplicate exists"),
        ));
    }
    let required = required_names
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    if let Some(name) = required.difference(&unique).next() {
        return Err(ActivationError::MissingName(name.clone()));
    }
    if let Some(name) = unique.difference(&required).next() {
        return Err(ActivationError::UnexpectedName(name.clone()));
    }
    Ok(names)
}

#[cfg(target_os = "linux")]
fn clear_activation_environment() {
    // SAFETY: from_environment is documented and used only during single-threaded process startup,
    // before any product worker or Tokio task can concurrently inspect or modify the environment.
    unsafe {
        env::remove_var("LISTEN_PID");
        env::remove_var("LISTEN_FDS");
        env::remove_var("LISTEN_FDNAMES");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_descriptors_are_order_independent() {
        assert_eq!(
            parse_names(2, "capulus:application", &["application", "capulus"]).unwrap(),
            ["capulus", "application"]
        );
    }

    #[test]
    fn duplicate_missing_and_unexpected_names_are_rejected() {
        assert!(matches!(
            parse_names(2, "capulus:capulus", &["application", "capulus"]),
            Err(ActivationError::DuplicateName(_))
        ));
        assert!(matches!(
            parse_names(1, "capulus", &["application", "capulus"]),
            Err(ActivationError::MissingName(_))
        ));
        assert!(matches!(
            parse_names(2, "capulus:other", &["application", "capulus"]),
            Err(ActivationError::MissingName(_))
        ));
    }
}
