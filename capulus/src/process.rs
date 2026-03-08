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

    use super::{render_command, require_success, run_with_input};

    #[test]
    fn render_command_quotes_args_with_whitespace() {
        let mut command = Command::new("ssh");
        command.args(["user@example.com", "echo hello world"]);

        assert_eq!(
            "ssh user@example.com 'echo hello world'",
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
