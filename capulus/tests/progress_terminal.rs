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
    fn start() -> Self {
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["terminal_fixture", "--exact", "--nocapture"])
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

#[test]
fn permanent_output_and_child_prompt_have_real_line_boundaries_after_resize() {
    let mut child = TerminalChild::start();
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
            assert_eq!(
                parser.screen().cursor_position().1,
                8,
                "prompt did not start at column zero"
            );
            break;
        }
    }
    let text = String::from_utf8(captured).unwrap();
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
