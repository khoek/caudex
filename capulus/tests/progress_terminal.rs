#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use capulus::ui::{CancellationMode, ColorMode, TaskOptions, TaskVisibility, Ui, UiOptions};

const FIXTURE: &str = "CAPULUS_PROGRESS_TERMINAL_FIXTURE";

#[test]
fn terminal_fixture() {
    if std::env::var_os(FIXTURE).is_none() {
        return;
    }
    let ui = Ui::from_options(UiOptions {
        cancellation: CancellationMode::Passive,
        color: ColorMode::Never,
        ..Default::default()
    })
    .unwrap();
    ui.info("FIRST");
    ui.detail("SECOND");
    ui.task(TaskOptions {
        label: "Quick operation".into(),
        ..Default::default()
    })
    .unwrap()
    .finish("FAST_COMPLETED");
    let task = ui
        .task(TaskOptions {
            label: "Waiting for a remote installation with a long descriptive status".into(),
            visibility: TaskVisibility::Immediate,
            ..Default::default()
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    ui.info("RESIZE_READY");
    ui.suspend(|| {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        assert_eq!(input.trim(), "continue");
        eprintln!("CHILD_OUTPUT");
    });
    task.finish("SLOW_COMPLETED");
    let group = ui.live_group("Installing fleet").unwrap();
    group
        .row("host", "installing")
        .unwrap()
        .finish("ROW_COMPLETED");
    group.finish("GROUP_COMPLETED");
    let transient = ui
        .task(TaskOptions {
            label: "TRANSIENT_OPERATION".into(),
            visibility: TaskVisibility::Immediate,
            ..Default::default()
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    transient.set_phase("UPDATED_OPERATION");
    std::thread::sleep(Duration::from_millis(150));
    transient.finish_and_clear();
    ui.success("LAST");
    drop(group);
    drop(ui);
    eprint!("PROMPT> ");
    std::io::stderr().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
}

struct TerminalChild {
    process: Child,
    master: File,
}

impl TerminalChild {
    fn start(mut command: Command) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // Successful openpty transfers ownership of both descriptors to the Files below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        command
            .env(FIXTURE, "1")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // The isolated child acquires the slave as its controlling terminal before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            process: command.spawn().unwrap(),
            master,
        }
    }

    fn resize(&self) {
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 40,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // The ioctl borrows a valid PTY descriptor and the initialized window size.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }
}

impl Drop for TerminalChild {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn capture_until_prompt(child: &mut TerminalChild) -> (vt100::Parser, Vec<u8>) {
    let mut parser = vt100::Parser::new(30, 100, 1000);
    let mut captured = Vec::new();
    let mut resized = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "terminal timed out: {}",
            parser.screen().contents()
        );
        let mut poll = libc::pollfd {
            fd: child.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Poll borrows one initialized descriptor with a bounded wait.
        if unsafe { libc::poll(&mut poll, 1, 50) } <= 0 {
            continue;
        }
        let mut bytes = [0; 8192];
        let size = child.master.read(&mut bytes).unwrap();
        captured.extend_from_slice(&bytes[..size]);
        parser.process(&bytes[..size]);
        if !resized && parser.screen().contents().contains("RESIZE_READY") {
            child.resize();
            parser.screen_mut().set_size(30, 40);
            child.master.write_all(b"continue\r").unwrap();
            resized = true;
        }
        if parser.screen().contents().contains("PROMPT>") {
            assert!(resized);
            break;
        }
    }
    (parser, captured)
}

#[test]
fn permanent_output_and_child_prompt_have_real_line_boundaries_after_resize() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["terminal_fixture", "--exact", "--nocapture"]);
    let mut child = TerminalChild::start(command);
    let (parser, captured) = capture_until_prompt(&mut child);
    assert_eq!(
        parser.screen().cursor_position().1,
        8,
        "prompt did not start at column zero"
    );
    let text = String::from_utf8(captured).unwrap();
    for marker in ["TRANSIENT_OPERATION", "UPDATED_OPERATION"] {
        assert!(text.contains(marker), "terminal never rendered {marker}");
        assert!(
            !parser.screen().contents().contains(marker),
            "completed progress remained on screen: {marker}"
        );
    }
    for marker in ["[start]", "[done]", "[phase]", "[wait]"] {
        assert!(
            !text.contains(marker),
            "terminal used plain progress: {marker}"
        );
    }
    for line in [
        "info: FIRST",
        "    SECOND",
        "info: RESIZE_READY",
        "ok: LAST",
    ] {
        assert!(
            text.contains(&format!("{line}\r\n")),
            "missing line boundary after {line}"
        );
    }
    for message in [
        "FAST_COMPLETED",
        "SLOW_COMPLETED",
        "ROW_COMPLETED",
        "GROUP_COMPLETED",
    ] {
        let remainder = text.split_once(message).unwrap().1;
        let elapsed = remainder
            .split_once("\r\n")
            .expect("completion needs a newline")
            .0;
        assert!(
            !elapsed.contains('\x1b') && elapsed.len() < 30,
            "completion wrapped into later output: {message}"
        );
    }
}

#[test]
#[ignore = "requires the zellij executable"]
fn zellij_scrollback_preserves_completed_lines_after_resize() {
    struct Session(String);
    impl Drop for Session {
        fn drop(&mut self) {
            for operation in ["kill-session", "delete-session"] {
                let _ = capulus::process::CaptureOptions {
                    timeout: Duration::from_secs(5),
                    ..Default::default()
                }
                .validate()
                .unwrap()
                .run(Command::new("zellij").args([operation, &self.0]), None);
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.kdl");
    std::fs::write(
        &config,
        "pane_frames false\ndefault_layout \"compact\"\nshow_startup_tips false\nshow_release_notes false\n",
    )
    .unwrap();
    let session = Session(format!("capulus-terminal-test-{}", std::process::id()));
    let mut command = Command::new("zellij");
    command
        .arg("--config")
        .arg(config)
        .args(["attach", "--create", &session.0, "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["terminal_fixture", "--exact", "--nocapture"]);
    let mut child = TerminalChild::start(command);
    capture_until_prompt(&mut child);
    let screen = capulus::process::CaptureOptions {
        timeout: Duration::from_secs(5),
        ..Default::default()
    }
    .validate()
    .unwrap()
    .run(
        Command::new("zellij").args(["--session", &session.0, "action", "dump-screen", "--full"]),
        None,
    )
    .unwrap();
    assert!(screen.status.success(), "{}", screen.stderr);
    for message in [
        "info: FIRST",
        "    SECOND",
        "info: RESIZE_READY",
        "ok: LAST",
        "PROMPT>",
    ] {
        assert!(
            screen.stdout.lines().any(|line| line.trim_end() == message),
            "{message} lost its line boundary:\n{}",
            screen.stdout
        );
    }
    for (message, prefix) in [
        ("FAST_COMPLETED", "✓ FAST_COMPLETED"),
        ("SLOW_COMPLETED", "✓ SLOW_COMPLETED"),
        ("ROW_COMPLETED", "✓ host · ROW_COMPLETED"),
        ("GROUP_COMPLETED", "✓ GROUP_COMPLETED"),
    ] {
        let line = screen
            .stdout
            .lines()
            .find(|line| line.contains(message))
            .unwrap();
        assert!(line.starts_with(prefix), "{line}");
        assert_eq!(line.matches("COMPLETED").count(), 1, "{line}");
    }
}
