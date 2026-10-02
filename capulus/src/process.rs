use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::process::{Command, ExitStatus, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::shell::shell_quote;

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug)]
pub struct CaptureOptions {
    pub timeout: std::time::Duration,
    pub max_output_bytes: u64,
    pub cancellation: crate::Cancellation,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            timeout: std::time::Duration::from_secs(60),
            max_output_bytes: 16 * 1024 * 1024,
            cancellation: crate::Cancellation::passive(),
        }
    }
}

pub struct CaptureConfig(CaptureOptions);

impl CaptureOptions {
    pub fn validate(self) -> Result<CaptureConfig> {
        anyhow::ensure!(!self.timeout.is_zero(), "command timeout must be positive");
        anyhow::ensure!(
            self.max_output_bytes > 0,
            "command output limit must be positive"
        );
        Ok(CaptureConfig(self))
    }
}

impl CaptureConfig {
    /// Capture a child and its descendants with bounded output and a deadline.
    pub fn run(&self, command: &mut Command, input: Option<&[u8]>) -> Result<CommandOutput> {
        self.execute(command, input, false)
    }

    /// Mirror both child output streams to stderr while retaining bounded diagnostics.
    /// The caller must suspend any interactive renderer before calling this method.
    pub fn run_streaming(
        &self,
        command: &mut Command,
        input: Option<&[u8]>,
    ) -> Result<CommandOutput> {
        self.execute(command, input, true)
    }

    fn execute(
        &self,
        command: &mut Command,
        input: Option<&[u8]>,
        streaming: bool,
    ) -> Result<CommandOutput> {
        use std::io::{Read, Seek};
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};

        self.0.cancellation.check()?;
        let mut stdin = tempfile::tempfile()?;
        stdin.write_all(input.unwrap_or_default())?;
        stdin.rewind()?;
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        command
            .process_group(0)
            .stdin(stdin)
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
        let mut child = CapturedChild(command.spawn().context("failed to start subprocess")?);
        let started = Instant::now();
        let mut positions = [0, 0];
        let status = loop {
            if streaming {
                mirror_output([&stdout, &stderr], &mut positions)?;
            }
            self.0.cancellation.check()?;
            anyhow::ensure!(
                started.elapsed() < self.0.timeout,
                "subprocess deadline exceeded after {:?}",
                self.0.timeout
            );
            anyhow::ensure!(
                stdout.metadata()?.len() <= self.0.max_output_bytes
                    && stderr.metadata()?.len() <= self.0.max_output_bytes,
                "subprocess output limit exceeded"
            );
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            self.0.cancellation.sleep(Duration::from_millis(50))?;
        };
        // Descendants must not outlive the captured command or continue writing its output.
        drop(child);
        if streaming {
            while positions[0] < stdout.metadata()?.len() || positions[1] < stderr.metadata()?.len()
            {
                self.0.cancellation.check()?;
                mirror_output([&stdout, &stderr], &mut positions)?;
            }
        }
        stdout.rewind()?;
        stderr.rewind()?;
        let mut out = String::new();
        let mut err = String::new();
        stdout
            .take(self.0.max_output_bytes + 1)
            .read_to_string(&mut out)?;
        stderr
            .take(self.0.max_output_bytes + 1)
            .read_to_string(&mut err)?;
        anyhow::ensure!(
            out.len() as u64 <= self.0.max_output_bytes
                && err.len() as u64 <= self.0.max_output_bytes,
            "subprocess output limit exceeded"
        );
        Ok(CommandOutput {
            status,
            stdout: out,
            stderr: err,
        })
    }
}

fn mirror_output(files: [&std::fs::File; 2], positions: &mut [u64; 2]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    let mut buffer = [0; 8192];
    let mut output = io::stderr().lock();
    for (file, position) in files.into_iter().zip(positions) {
        // Limit work per tick so a continuously writing child cannot postpone cancellation.
        for _ in 0..16 {
            let count = file.read_at(&mut buffer, *position)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            *position += count as u64;
        }
    }
    output.flush()?;
    Ok(())
}

struct CapturedChild(std::process::Child);

impl Drop for CapturedChild {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.0.id()) {
            // The child creates its own process group before exec; a negative PID targets that group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}

pub fn ensure_command_available(program: &str) -> Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .with_context(|| format!("`{program}` is required but was not found in PATH"))?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "`{program} --version` exited with status {}",
            output.status.code().unwrap_or(1)
        )
    }
}

pub fn render_command(command: &Command) -> String {
    let program = command.get_program().to_string_lossy();
    let args = command
        .get_args()
        .map(os_to_display)
        .collect::<Vec<_>>()
        .join(" ");
    if args.is_empty() {
        program.into_owned()
    } else {
        format!("{program} {args}")
    }
}

pub fn run_capture(command: &mut Command) -> Result<CommandOutput> {
    let rendered = render_command(command);
    let output = command
        .output()
        .with_context(|| format!("failed to run `{rendered}`"))?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn run_output(command: &mut Command, action: &str) -> Result<Output> {
    let rendered = render_command(command);
    let output = command
        .output()
        .with_context(|| format!("failed to run `{rendered}`"))?;
    if output.status.success() {
        Ok(output)
    } else {
        let detail = failure_detail_from_output(&output);
        bail!("Failed to {action} while running `{rendered}`: {detail}")
    }
}

pub fn run_with_input(command: &mut Command, input: &[u8]) -> Result<CommandOutput> {
    let rendered = render_command(command);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run `{rendered}`"))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(input)
            .with_context(|| format!("failed to pipe stdin into `{rendered}`"))?;
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed waiting for `{rendered}`"))?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

pub fn require_success(action: &str, command: &mut Command) -> Result<CommandOutput> {
    let rendered = render_command(command);
    let output = run_capture(command)?;
    if output.status.success() {
        return Ok(output);
    }
    bail!(
        "Failed to {action} while running `{rendered}`: {}",
        failure_detail(&output)
    )
}

pub fn require_success_with_input(
    action: &str,
    command: &mut Command,
    input: &[u8],
) -> Result<CommandOutput> {
    let rendered = render_command(command);
    let output = run_with_input(command, input)?;
    if output.status.success() {
        return Ok(output);
    }
    bail!(
        "Failed to {action} while running `{rendered}`: {}",
        failure_detail(&output)
    )
}

pub fn run_status(command: &mut Command, action: &str) -> Result<()> {
    require_success(action, command).map(|_| ())
}

pub fn run_status_streaming(command: &mut Command, action: &str) -> Result<()> {
    if !(io::stdout().is_terminal() || io::stderr().is_terminal()) {
        return run_status(command, action);
    }

    let rendered = render_command(command);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    let status = command
        .status()
        .with_context(|| format!("failed to run `{rendered}`"))?;
    if status.success() {
        Ok(())
    } else {
        bail!(
            "Failed to {action} while running `{rendered}`: exit status {}",
            status.code().unwrap_or(1)
        )
    }
}

pub fn run_status_with_input(command: &mut Command, action: &str, input: &[u8]) -> Result<()> {
    require_success_with_input(action, command, input).map(|_| ())
}

pub fn run_text(command: &mut Command, action: &str) -> Result<String> {
    Ok(require_success(action, command)?.stdout.trim().to_owned())
}

pub fn run_json_value(command: &mut Command, action: &str) -> Result<Value> {
    run_json(command, action)
}

pub fn run_json<T: DeserializeOwned>(command: &mut Command, action: &str) -> Result<T> {
    let output = require_success(action, command)?;
    serde_json::from_str::<T>(&output.stdout).with_context(|| {
        format!(
            "Failed to parse JSON while trying to {action}: {}",
            truncate_ellipsis(output.stdout.trim(), 280)
        )
    })
}

pub fn run_status_code(command: &mut Command) -> Result<i32> {
    let rendered = render_command(command);
    let status = command
        .status()
        .with_context(|| format!("failed to run `{rendered}`"))?;
    Ok(status.code().unwrap_or(1))
}

fn failure_detail(output: &CommandOutput) -> String {
    let stderr = output.stderr.trim();
    let stdout = output.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_owned()
    } else if !stdout.is_empty() {
        stdout.to_owned()
    } else {
        format!("exit status {}", output.status.code().unwrap_or(1))
    }
}

fn failure_detail_from_output(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if !stderr.is_empty() {
        stderr.to_owned()
    } else if !stdout.is_empty() {
        stdout.to_owned()
    } else {
        format!("exit status {}", output.status.code().unwrap_or(1))
    }
}

fn os_to_display(value: &OsStr) -> String {
    shell_quote(&value.to_string_lossy())
}

fn truncate_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    #[test]
    fn bounded_capture_transmits_input_and_preserves_exit_status() {
        let output = super::CaptureOptions::default()
            .validate()
            .unwrap()
            .run(
                Command::new("sh").args(["-c", "cat; printf diagnostic >&2; exit 7"]),
                Some(b"payload\n"),
            )
            .unwrap();
        assert_eq!(output.stdout, "payload\n");
        assert_eq!(output.stderr, "diagnostic");
        assert_eq!(output.status.code(), Some(7));
        let error = super::CaptureOptions {
            max_output_bytes: 32,
            ..Default::default()
        }
        .validate()
        .unwrap()
        .run(Command::new("head").args(["-c", "1024", "/dev/zero"]), None)
        .unwrap_err();
        assert!(error.to_string().contains("output limit"));
    }

    #[test]
    fn capture_deadline_kills_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("escaped");
        let error = super::CaptureOptions {
            timeout: std::time::Duration::from_millis(100),
            ..Default::default()
        }
        .validate()
        .unwrap()
        .run(
            Command::new("sh")
                .args(["-c", "(sleep 0.4; touch \"$1\") & wait", "test"])
                .arg(&marker),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline"));
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!marker.exists(), "descendant survived its command deadline");
    }

    #[test]
    fn capture_cancellation_remains_typed() {
        if std::env::var_os("CAPULUS_TEST_CAPTURE_SIGNAL").is_some() {
            let cancellation = crate::Cancellation::install().unwrap();
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGINT);
                }
            });
            let error = super::CaptureOptions {
                cancellation,
                ..Default::default()
            }
            .validate()
            .unwrap()
            .run(Command::new("sleep").arg("30"), None)
            .unwrap_err()
            .context("operation stopped");
            assert!(crate::error_is_cancelled(&error));
            return;
        }
        let result = super::CaptureOptions {
            timeout: std::time::Duration::from_secs(10),
            ..Default::default()
        }
        .validate()
        .unwrap()
        .run(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::tests::capture_cancellation_remains_typed",
                    "--nocapture",
                ])
                .env("CAPULUS_TEST_CAPTURE_SIGNAL", "1"),
            None,
        )
        .unwrap();
        assert!(result.status.success(), "{}", result.stderr);
    }

    use super::{render_command, require_success, run_with_input};

    #[test]
    fn render_command_quotes_args_with_whitespace() {
        let mut command = Command::new("ssh");
        command.args(["user@example.com", "echo hello world"]);

        assert_eq!(
            "ssh 'user@example.com' 'echo hello world'",
            render_command(&command)
        );
    }

    #[test]
    fn run_with_input_pipes_stdin_to_child() {
        let mut command = Command::new("cat");
        let output = run_with_input(&mut command, b"abc\n").expect("cat should succeed");

        assert!(output.status.success());
        assert_eq!("abc\n", output.stdout);
    }

    #[test]
    fn require_success_reports_stdout_when_stderr_is_empty() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'stdout-only'; exit 2"]);

        let error =
            require_success("run failing command", &mut command).expect_err("command should fail");
        let text = error.to_string();
        assert!(text.contains("stdout-only"));
    }
}
