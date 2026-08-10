use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);
pub(crate) const INTERRUPTED_EXIT_CODE: i32 = 128 + libc::SIGINT;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static INSTALL_RESULT: OnceLock<Result<(), i32>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default)]
pub struct Cancellation;

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("operation interrupted")]
pub struct Cancelled;

pub fn error_is_cancelled(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Cancelled>().is_some())
}

impl Cancellation {
    pub const fn passive() -> Self {
        Self
    }

    pub fn install() -> io::Result<Self> {
        let result = INSTALL_RESULT.get_or_init(install_sigint_handler);
        match result {
            Ok(()) => Ok(Self),
            Err(error) => Err(io::Error::from_raw_os_error(*error)),
        }
    }

    pub fn is_requested(self) -> bool {
        INTERRUPTED.load(Ordering::SeqCst)
    }

    pub fn check(self) -> Result<(), Cancelled> {
        if self.is_requested() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn sleep(self, duration: Duration) -> Result<(), Cancelled> {
        let deadline = Instant::now() + duration;
        loop {
            self.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            thread::sleep(remaining.min(CANCELLATION_POLL_INTERVAL));
        }
    }
}

extern "C" fn handle_sigint(_signal: libc::c_int) {
    if INTERRUPTED.swap(true, Ordering::SeqCst) {
        // A second Ctrl-C is an explicit request to stop immediately. `_exit` is
        // async-signal-safe and avoids running arbitrary process teardown here.
        unsafe { libc::_exit(INTERRUPTED_EXIT_CODE) };
    }
}

fn install_sigint_handler() -> Result<(), i32> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handle_sigint as *const () as usize;
    action.sa_flags = 0;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if unsafe { libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncancelled_sleep_reaches_its_deadline() {
        let started = Instant::now();
        Cancellation
            .sleep(Duration::from_millis(2))
            .expect("sleep should not be cancelled");
        assert!(started.elapsed() >= Duration::from_millis(2));
    }

    #[test]
    fn cancellation_remains_typed_through_context() {
        let error = anyhow::Error::new(Cancelled).context("state remains committed");
        assert!(error_is_cancelled(&error));
        assert!(!error_is_cancelled(&anyhow::anyhow!("ordinary failure")));
    }
}
