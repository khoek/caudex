use std::env;
use std::fmt;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::Cancellation;

const TICK_INTERVAL: Duration = Duration::from_millis(90);
const SPINNER_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgressMode {
    #[default]
    Auto,
    Interactive,
    Plain,
    Off,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CancellationMode {
    #[default]
    Signal,
    Passive,
}

#[derive(Clone, Debug)]
pub struct UiOptions {
    pub progress: ProgressMode,
    pub color: ColorMode,
    pub cancellation: CancellationMode,
    pub visibility_delay: Duration,
    pub heartbeat_interval: Duration,
}

impl Default for UiOptions {
    fn default() -> Self {
        Self {
            progress: ProgressMode::Auto,
            color: ColorMode::Auto,
            cancellation: CancellationMode::Signal,
            visibility_delay: Duration::from_millis(200),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

impl UiOptions {
    pub fn validate(self) -> Result<ValidatedUiOptions> {
        self.validate_for(TerminalCapabilities::detect())
    }

    fn validate_for(self, terminal: TerminalCapabilities) -> Result<ValidatedUiOptions> {
        if self.heartbeat_interval.is_zero() {
            bail!("UI heartbeat interval must be greater than zero");
        }
        let progress = match self.progress {
            ProgressMode::Auto if terminal.stderr => ResolvedProgressMode::Interactive,
            ProgressMode::Auto => ResolvedProgressMode::Plain,
            ProgressMode::Interactive if terminal.stderr => ResolvedProgressMode::Interactive,
            ProgressMode::Interactive => {
                bail!("interactive progress requires stderr to be attached to a terminal")
            }
            ProgressMode::Plain => ResolvedProgressMode::Plain,
            ProgressMode::Off => ResolvedProgressMode::Off,
        };
        let no_color = env::var_os("NO_COLOR").is_some();
        let color = match self.color {
            ColorMode::Auto => terminal.stderr && !no_color,
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        let stdout_color = match self.color {
            ColorMode::Auto => terminal.stdout && !no_color,
            ColorMode::Always => true,
            ColorMode::Never => false,
        };
        Ok(ValidatedUiOptions {
            progress,
            color,
            stdout_color,
            cancellation: self.cancellation,
            visibility_delay: self.visibility_delay,
            heartbeat_interval: self.heartbeat_interval,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalCapabilities {
    stderr: bool,
    stdout: bool,
}

impl TerminalCapabilities {
    fn detect() -> Self {
        Self {
            stderr: io::stderr().is_terminal(),
            stdout: io::stdout().is_terminal(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedUiOptions {
    progress: ResolvedProgressMode,
    color: bool,
    stdout_color: bool,
    cancellation: CancellationMode,
    visibility_delay: Duration,
    heartbeat_interval: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedProgressMode {
    Interactive,
    Plain,
    Off,
}

#[derive(Clone)]
pub struct Ui {
    inner: Arc<UiInner>,
}

struct UiInner {
    options: ValidatedUiOptions,
    progress: Option<Arc<MultiProgress>>,
    output: Arc<dyn LineOutput>,
    cancellation: Cancellation,
}

trait LineOutput: Send + Sync {
    fn write_line(&self, line: &str);
}

struct StderrOutput;

impl LineOutput for StderrOutput {
    fn write_line(&self, line: &str) {
        eprintln!("{line}");
    }
}

impl Ui {
    pub fn new(options: ValidatedUiOptions) -> Result<Self> {
        let cancellation = match options.cancellation {
            CancellationMode::Signal => Cancellation::install()?,
            CancellationMode::Passive => Cancellation::passive(),
        };
        let progress = matches!(options.progress, ResolvedProgressMode::Interactive)
            .then(|| Arc::new(MultiProgress::new()));
        Ok(Self {
            inner: Arc::new(UiInner {
                options,
                progress,
                output: Arc::new(StderrOutput),
                cancellation,
            }),
        })
    }

    pub fn from_options(options: UiOptions) -> Result<Self> {
        Self::new(options.validate()?)
    }

    pub fn is_interactive(&self) -> bool {
        matches!(
            self.inner.options.progress,
            ResolvedProgressMode::Interactive
        )
    }

    pub fn progress_is_enabled(&self) -> bool {
        !matches!(self.inner.options.progress, ResolvedProgressMode::Off)
    }

    pub fn color_is_enabled(&self) -> bool {
        self.inner.options.color
    }

    pub fn stdout_color_is_enabled(&self) -> bool {
        self.inner.options.stdout_color
    }

    pub fn cancellation(&self) -> Cancellation {
        self.inner.cancellation
    }

    pub fn check_cancelled(&self) -> std::result::Result<(), crate::Cancelled> {
        self.inner.cancellation.check()
    }

    pub fn sleep(&self, duration: Duration) -> std::result::Result<(), crate::Cancelled> {
        self.inner.cancellation.sleep(duration)
    }

    pub fn task(&self, options: TaskOptions) -> Result<Task> {
        let options = options.validate()?;
        let shared = Arc::new(TaskShared {
            state: Mutex::new(TaskState::new(options)),
            changed: Condvar::new(),
        });
        let renderer = match self.inner.options.progress {
            ResolvedProgressMode::Interactive => Some(spawn_interactive_renderer(
                Arc::clone(&shared),
                Arc::clone(
                    self.inner
                        .progress
                        .as_ref()
                        .expect("interactive UI owns a MultiProgress"),
                ),
                self.inner.options.visibility_delay,
                self.inner.options.color,
            )),
            ResolvedProgressMode::Plain => Some(spawn_plain_renderer(
                Arc::clone(&shared),
                Arc::clone(&self.inner.output),
                self.inner.options.visibility_delay,
                self.inner.options.heartbeat_interval,
            )),
            ResolvedProgressMode::Off => None,
        };
        Ok(Task { shared, renderer })
    }

    pub fn live_group(&self, label: impl Into<String>) -> Result<LiveGroup> {
        LiveGroup::new(self.clone(), label.into())
    }

    pub fn run<T>(&self, options: TaskOptions, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let task = self.task(options)?;
        match operation() {
            Ok(value) => {
                task.finish_and_clear();
                Ok(value)
            }
            Err(error) if crate::error_is_cancelled(&error) => {
                let label = task.label();
                task.abandon(format!("{label} interrupted"));
                Err(error)
            }
            Err(error) => {
                task.fail(error.to_string());
                Err(error)
            }
        }
    }

    pub fn suspend<T>(&self, operation: impl FnOnce() -> T) -> T {
        match &self.inner.progress {
            Some(progress) => progress.suspend(operation),
            None => operation(),
        }
    }

    pub fn info(&self, message: impl fmt::Display) {
        self.line("info", message, "\x1b[1;36m");
    }

    pub fn success(&self, message: impl fmt::Display) {
        self.line("ok", message, "\x1b[1;32m");
    }

    pub fn warn(&self, message: impl fmt::Display) {
        self.line("warning", message, "\x1b[1;33m");
    }

    pub fn error(&self, message: impl fmt::Display) {
        self.line("error", message, "\x1b[1;31m");
    }

    pub fn detail(&self, message: impl fmt::Display) {
        self.write_line(&format!("    {message}"));
    }

    fn line(&self, label: &str, message: impl fmt::Display, ansi: &str) {
        if self.inner.options.color {
            self.write_line(&format!("{ansi}{label}:\x1b[0m {message}"));
        } else {
            self.write_line(&format!("{label}: {message}"));
        }
    }

    fn write_line(&self, line: &str) {
        match &self.inner.progress {
            Some(progress) => {
                let _ = progress.println(line);
            }
            None => self.inner.output.write_line(line),
        }
    }

    #[cfg(test)]
    fn with_output(options: ValidatedUiOptions, output: Arc<dyn LineOutput>) -> Self {
        Self {
            inner: Arc::new(UiInner {
                progress: None,
                options,
                output,
                cancellation: Cancellation,
            }),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TaskOptions {
    pub label: String,
    pub kind: TaskKind,
    pub deadline: Option<Duration>,
    pub visibility: TaskVisibility,
}

impl TaskOptions {
    fn validate(self) -> Result<ValidatedTaskOptions> {
        let label = self.label.trim();
        if label.is_empty() {
            bail!("UI task label must not be empty");
        }
        match &self.kind {
            TaskKind::Indeterminate => {}
            TaskKind::Counter { total, .. } | TaskKind::Countdown { total } if *total == 0 => {
                bail!("UI task total must be greater than zero")
            }
            TaskKind::Counter { .. } | TaskKind::Countdown { .. } => {}
        }
        if self.deadline.is_some_and(|deadline| deadline.is_zero()) {
            bail!("UI task deadline must be greater than zero");
        }
        Ok(ValidatedTaskOptions {
            label: label.to_owned(),
            kind: self.kind,
            deadline: self.deadline,
            visibility: self.visibility,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub enum TaskKind {
    #[default]
    Indeterminate,
    Counter {
        total: u64,
        unit: Option<String>,
    },
    Countdown {
        total: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TaskVisibility {
    Immediate,
    #[default]
    Delayed,
}

#[derive(Clone, Debug)]
struct ValidatedTaskOptions {
    label: String,
    kind: TaskKind,
    deadline: Option<Duration>,
    visibility: TaskVisibility,
}

pub struct Task {
    shared: Arc<TaskShared>,
    renderer: Option<JoinHandle<()>>,
}

impl Task {
    fn label(&self) -> String {
        self.shared
            .state
            .lock()
            .expect("task state lock")
            .options
            .label
            .clone()
    }

    pub fn set_phase(&self, phase: impl Into<String>) {
        let mut state = self.shared.state.lock().expect("task state lock");
        state.phase = Some(phase.into());
        state.phase_revision = state.phase_revision.wrapping_add(1);
        self.shared.changed.notify_all();
    }

    pub fn set_detail(&self, detail: impl Into<String>) {
        let mut state = self.shared.state.lock().expect("task state lock");
        state.detail = Some(detail.into());
        self.shared.changed.notify_all();
    }

    pub fn clear_detail(&self) {
        let mut state = self.shared.state.lock().expect("task state lock");
        state.detail = None;
        self.shared.changed.notify_all();
    }

    pub fn set_position(&self, position: u64) {
        let mut state = self.shared.state.lock().expect("task state lock");
        state.position = position.min(state.total().unwrap_or(u64::MAX));
        self.shared.changed.notify_all();
    }

    pub fn inc(&self, delta: u64) {
        let mut state = self.shared.state.lock().expect("task state lock");
        state.position = state
            .position
            .saturating_add(delta)
            .min(state.total().unwrap_or(u64::MAX));
        self.shared.changed.notify_all();
    }

    pub fn elapsed(&self) -> Duration {
        self.shared
            .state
            .lock()
            .expect("task state lock")
            .started
            .elapsed()
    }

    pub fn finish(self, message: impl Into<String>) {
        self.complete(TaskOutcome::Success(message.into()));
    }

    pub fn finish_and_clear(self) {
        self.complete(TaskOutcome::Clear);
    }

    pub fn fail(self, message: impl Into<String>) {
        self.complete(TaskOutcome::Failure(message.into()));
    }

    pub fn abandon(self, message: impl Into<String>) {
        self.complete(TaskOutcome::Abandoned(message.into()));
    }

    fn complete(mut self, outcome: TaskOutcome) {
        set_outcome(&self.shared, outcome);
        join_renderer(self.renderer.take());
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if self
            .shared
            .state
            .lock()
            .expect("task state lock")
            .outcome
            .is_none()
        {
            set_outcome(&self.shared, TaskOutcome::Clear);
        }
        join_renderer(self.renderer.take());
    }
}

struct TaskShared {
    state: Mutex<TaskState>,
    changed: Condvar,
}

struct TaskState {
    options: ValidatedTaskOptions,
    phase: Option<String>,
    detail: Option<String>,
    position: u64,
    phase_revision: u64,
    started: Instant,
    outcome: Option<TaskOutcome>,
}

impl TaskState {
    fn new(options: ValidatedTaskOptions) -> Self {
        Self {
            options,
            phase: None,
            detail: None,
            position: 0,
            phase_revision: 0,
            started: Instant::now(),
            outcome: None,
        }
    }

    fn total(&self) -> Option<u64> {
        match self.options.kind {
            TaskKind::Indeterminate => None,
            TaskKind::Counter { total, .. } | TaskKind::Countdown { total } => Some(total),
        }
    }

    fn render_message(&self, now: Instant) -> String {
        let elapsed = now.saturating_duration_since(self.started);
        let mut message = self.options.label.clone();
        if let Some(phase) = self.phase.as_deref() {
            message.push_str(" · ");
            message.push_str(phase);
        }
        if let Some(detail) = self.detail.as_deref() {
            message.push_str(" · ");
            message.push_str(detail);
        }
        match &self.options.kind {
            TaskKind::Indeterminate => {}
            TaskKind::Counter { total, unit } => {
                message.push_str(&format!(" · {}/{total}", self.position));
                if let Some(unit) = unit.as_deref() {
                    message.push(' ');
                    message.push_str(unit);
                }
            }
            TaskKind::Countdown { total } => {
                let remaining = total.saturating_sub(self.position);
                message.push_str(&format!(" · {} remaining", format_seconds(remaining)));
            }
        }
        message.push_str(&format!(" · elapsed {}", format_duration(elapsed)));
        if let Some(deadline) = self.options.deadline {
            message.push_str(&format!(
                " · deadline in {}",
                format_duration(deadline.saturating_sub(elapsed))
            ));
        }
        message
    }
}

#[derive(Clone)]
enum TaskOutcome {
    Success(String),
    Failure(String),
    Abandoned(String),
    Clear,
}

fn set_outcome(shared: &TaskShared, outcome: TaskOutcome) {
    let mut state = shared.state.lock().expect("task state lock");
    if state.outcome.is_none() {
        state.outcome = Some(outcome);
        shared.changed.notify_all();
    }
}

fn join_renderer(renderer: Option<JoinHandle<()>>) {
    if let Some(renderer) = renderer {
        let _ = renderer.join();
    }
}

fn spawn_interactive_renderer(
    shared: Arc<TaskShared>,
    progress: Arc<MultiProgress>,
    default_delay: Duration,
    color: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let delay = task_visibility_delay(&shared, default_delay);
        if wait_until_visible_or_complete(&shared, delay) {
            render_fast_interactive_completion(&shared, &progress);
            return;
        }

        let state = shared.state.lock().expect("task state lock");
        let bar = match state.options.kind {
            TaskKind::Indeterminate => ProgressBar::new_spinner(),
            TaskKind::Counter { total, .. } | TaskKind::Countdown { total } => {
                ProgressBar::new(total)
            }
        };
        bar.set_style(task_style(&state.options.kind, color));
        bar.set_message(state.render_message(Instant::now()));
        bar.set_position(state.position);
        if matches!(state.options.kind, TaskKind::Indeterminate) {
            bar.enable_steady_tick(TICK_INTERVAL);
        }
        drop(state);
        let bar = progress.add(bar);

        loop {
            let state = shared.state.lock().expect("task state lock");
            bar.set_message(state.render_message(Instant::now()));
            bar.set_position(state.position);
            if let Some(outcome) = state.outcome.clone() {
                let elapsed = state.started.elapsed();
                drop(state);
                finish_interactive_bar(&bar, outcome, elapsed);
                return;
            }
            let _ = shared
                .changed
                .wait_timeout(state, TICK_INTERVAL)
                .expect("task state wait");
        }
    })
}

fn spawn_plain_renderer(
    shared: Arc<TaskShared>,
    output: Arc<dyn LineOutput>,
    default_delay: Duration,
    heartbeat: Duration,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let delay = task_visibility_delay(&shared, default_delay);
        if wait_until_visible_or_complete(&shared, delay) {
            render_fast_completion(&shared, output.as_ref());
            return;
        }

        let mut state = shared.state.lock().expect("task state lock");
        output.write_line(&format!("[start] {}", state.render_message(Instant::now())));
        let mut phase_revision = state.phase_revision;
        let mut last_heartbeat = Instant::now();
        loop {
            if let Some(outcome) = state.outcome.clone() {
                render_plain_outcome(output.as_ref(), &state, outcome);
                return;
            }
            let wait = heartbeat.saturating_sub(last_heartbeat.elapsed());
            let (next, _) = shared
                .changed
                .wait_timeout(state, wait)
                .expect("task state wait");
            state = next;
            if state.phase_revision != phase_revision {
                output.write_line(&format!("[phase] {}", state.render_message(Instant::now())));
                phase_revision = state.phase_revision;
                last_heartbeat = Instant::now();
            } else if last_heartbeat.elapsed() >= heartbeat {
                output.write_line(&format!("[wait] {}", state.render_message(Instant::now())));
                last_heartbeat = Instant::now();
            }
        }
    })
}

fn task_visibility_delay(shared: &TaskShared, default_delay: Duration) -> Duration {
    match shared
        .state
        .lock()
        .expect("task state lock")
        .options
        .visibility
    {
        TaskVisibility::Immediate => Duration::ZERO,
        TaskVisibility::Delayed => default_delay,
    }
}

fn wait_until_visible_or_complete(shared: &TaskShared, delay: Duration) -> bool {
    let state = shared.state.lock().expect("task state lock");
    if state.outcome.is_some() {
        return true;
    }
    if delay.is_zero() {
        return false;
    }
    let (state, _) = shared
        .changed
        .wait_timeout_while(state, delay, |state| state.outcome.is_none())
        .expect("task visibility wait");
    state.outcome.is_some()
}

fn render_fast_completion(shared: &TaskShared, output: &dyn LineOutput) {
    let state = shared.state.lock().expect("task state lock");
    if let Some(outcome) = state.outcome.clone() {
        match outcome {
            TaskOutcome::Success(_) | TaskOutcome::Failure(_) | TaskOutcome::Abandoned(_) => {
                render_plain_outcome(output, &state, outcome)
            }
            TaskOutcome::Clear => {}
        }
    }
}

fn render_fast_interactive_completion(shared: &TaskShared, progress: &MultiProgress) {
    let state = shared.state.lock().expect("task state lock");
    let elapsed = format_duration(state.started.elapsed());
    let line = match state.outcome.as_ref() {
        Some(TaskOutcome::Success(message)) => Some(format!("✓ {message} · {elapsed}")),
        Some(TaskOutcome::Failure(message)) => Some(format!("✗ {message} · {elapsed}")),
        Some(TaskOutcome::Abandoned(message)) => Some(format!("! {message} · {elapsed}")),
        Some(TaskOutcome::Clear) | None => None,
    };
    if let Some(line) = line {
        let _ = progress.println(line);
    }
}

fn finish_interactive_bar(bar: &ProgressBar, outcome: TaskOutcome, elapsed: Duration) {
    bar.disable_steady_tick();
    match outcome {
        TaskOutcome::Success(message) => {
            bar.set_style(message_style());
            bar.finish_with_message(format!("✓ {message} · {}", format_duration(elapsed)));
        }
        TaskOutcome::Failure(message) => {
            bar.set_style(message_style());
            bar.abandon_with_message(format!("✗ {message} · {}", format_duration(elapsed)));
        }
        TaskOutcome::Abandoned(message) => {
            bar.set_style(message_style());
            bar.abandon_with_message(format!("! {message} · {}", format_duration(elapsed)));
        }
        TaskOutcome::Clear => bar.finish_and_clear(),
    }
}

fn render_plain_outcome(output: &dyn LineOutput, state: &TaskState, outcome: TaskOutcome) {
    let elapsed = format_duration(state.started.elapsed());
    match outcome {
        TaskOutcome::Success(message) => output.write_line(&format!("[ok] {message} · {elapsed}")),
        TaskOutcome::Failure(message) => {
            output.write_line(&format!("[fail] {message} · {elapsed}"));
        }
        TaskOutcome::Abandoned(message) => {
            output.write_line(&format!("[stop] {message} · {elapsed}"));
        }
        TaskOutcome::Clear => {
            output.write_line(&format!("[done] {} · {elapsed}", state.options.label))
        }
    }
}

fn task_style(kind: &TaskKind, color: bool) -> ProgressStyle {
    match kind {
        TaskKind::Indeterminate => ProgressStyle::with_template(if color {
            "{spinner:.cyan} {msg}"
        } else {
            "{spinner} {msg}"
        })
        .expect("task spinner template")
        .tick_strings(SPINNER_TICKS),
        TaskKind::Counter { .. } | TaskKind::Countdown { .. } => {
            ProgressStyle::with_template(if color {
                "[{bar:28.cyan/blue}] {msg}"
            } else {
                "[{bar:28}] {msg}"
            })
            .expect("task counter template")
            .progress_chars("=>-")
        }
    }
}

pub struct LiveGroup {
    inner: Arc<LiveGroupInner>,
}

struct LiveGroupInner {
    ui: Ui,
    footer: Mutex<Option<ProgressBar>>,
    last_plain_summary: Mutex<Instant>,
    started: Instant,
    finished: AtomicBool,
}

impl LiveGroup {
    fn new(ui: Ui, label: String) -> Result<Self> {
        let label = label.trim().to_owned();
        if label.is_empty() {
            bail!("UI live-group label must not be empty");
        }
        let footer = match ui.inner.options.progress {
            ResolvedProgressMode::Interactive => {
                let bar = ProgressBar::new_spinner();
                bar.set_style(live_footer_style(ui.color_is_enabled()));
                bar.set_message(label.clone());
                bar.enable_steady_tick(TICK_INTERVAL);
                Some(
                    ui.inner
                        .progress
                        .as_ref()
                        .expect("interactive UI owns a MultiProgress")
                        .add(bar),
                )
            }
            ResolvedProgressMode::Plain => {
                ui.write_line(&format!("[start] {label}"));
                None
            }
            ResolvedProgressMode::Off => None,
        };
        Ok(Self {
            inner: Arc::new(LiveGroupInner {
                ui,
                footer: Mutex::new(footer),
                last_plain_summary: Mutex::new(Instant::now()),
                started: Instant::now(),
                finished: AtomicBool::new(false),
            }),
        })
    }

    pub fn row(&self, label: impl Into<String>, phase: impl Into<String>) -> Result<LiveRow> {
        let label = label.into();
        let label = label.trim().to_owned();
        if label.is_empty() {
            bail!("UI live-row label must not be empty");
        }
        self.create_row(LiveRowPresentation::Labeled(label), phase.into())
    }

    /// Adds a row whose message is already fully rendered by the caller.
    ///
    /// This is intended for aligned tables and other live displays where
    /// splitting the row into a fixed label and phase would destroy layout.
    pub fn rendered_row(&self, message: impl Into<String>) -> Result<LiveRow> {
        let message = message.into();
        if message.trim().is_empty() {
            bail!("UI rendered live-row message must not be empty");
        }
        self.create_row(LiveRowPresentation::Rendered, message)
    }

    fn create_row(&self, presentation: LiveRowPresentation, message: String) -> Result<LiveRow> {
        let bar = match self.inner.ui.inner.options.progress {
            ResolvedProgressMode::Interactive => {
                let bar = ProgressBar::new_spinner();
                match &presentation {
                    LiveRowPresentation::Labeled(label) => {
                        bar.set_style(live_row_style(self.inner.ui.color_is_enabled()));
                        bar.set_prefix(label.clone());
                    }
                    LiveRowPresentation::Rendered => {
                        bar.set_style(live_rendered_row_style(self.inner.ui.color_is_enabled()));
                    }
                }
                bar.set_message(message.clone());
                bar.enable_steady_tick(TICK_INTERVAL);
                let progress = self
                    .inner
                    .ui
                    .inner
                    .progress
                    .as_ref()
                    .expect("interactive UI owns a MultiProgress");
                let footer = self.inner.footer.lock().expect("live-group footer lock");
                Some(match footer.as_ref() {
                    Some(footer) => progress.insert_before(footer, bar),
                    None => progress.add(bar),
                })
            }
            ResolvedProgressMode::Plain => {
                self.inner
                    .ui
                    .write_line(&format!("[item] {}", presentation.plain_message(&message)));
                None
            }
            ResolvedProgressMode::Off => None,
        };
        Ok(LiveRow {
            inner: Arc::new(LiveRowInner {
                ui: self.inner.ui.clone(),
                presentation,
                state: Mutex::new(LiveRowState { message, bar }),
                started: Instant::now(),
                finished: AtomicBool::new(false),
            }),
        })
    }

    pub fn set_summary(&self, message: impl Into<String>) {
        let message = message.into();
        if let Some(footer) = self
            .inner
            .footer
            .lock()
            .expect("live-group footer lock")
            .as_ref()
        {
            footer.set_message(message);
            return;
        }
        if matches!(
            self.inner.ui.inner.options.progress,
            ResolvedProgressMode::Plain
        ) {
            let mut last = self
                .inner
                .last_plain_summary
                .lock()
                .expect("live-group summary lock");
            if last.elapsed() >= self.inner.ui.inner.options.heartbeat_interval {
                self.inner.ui.write_line(&format!("[wait] {message}"));
                *last = Instant::now();
            }
        }
    }

    pub fn print_summary(&self, message: impl Into<String>) {
        let message = message.into();
        if let Some(footer) = self
            .inner
            .footer
            .lock()
            .expect("live-group footer lock")
            .as_ref()
        {
            footer.set_message(message);
        } else if matches!(
            self.inner.ui.inner.options.progress,
            ResolvedProgressMode::Plain
        ) {
            self.inner.ui.write_line(&format!("[status] {message}"));
            *self
                .inner
                .last_plain_summary
                .lock()
                .expect("live-group summary lock") = Instant::now();
        }
    }

    pub fn finish(&self, message: impl Into<String>) {
        self.complete("ok", "✓", message.into());
    }

    pub fn fail(&self, message: impl Into<String>) {
        self.complete("fail", "✗", message.into());
    }

    pub fn abandon(&self, message: impl Into<String>) {
        self.complete("stop", "!", message.into());
    }

    pub fn clear(&self) {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(footer) = self
            .inner
            .footer
            .lock()
            .expect("live-group footer lock")
            .take()
        {
            footer.finish_and_clear();
        }
    }

    fn complete(&self, plain_label: &str, glyph: &str, message: String) {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let elapsed = format_duration(self.inner.started.elapsed());
        if let Some(footer) = self
            .inner
            .footer
            .lock()
            .expect("live-group footer lock")
            .take()
        {
            footer.disable_steady_tick();
            footer.set_style(message_style());
            footer.finish_with_message(format!("{glyph} {message} · {elapsed}"));
        } else if matches!(
            self.inner.ui.inner.options.progress,
            ResolvedProgressMode::Plain
        ) {
            self.inner
                .ui
                .write_line(&format!("[{plain_label}] {message} · {elapsed}"));
        }
    }
}

impl Drop for LiveGroup {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 && !self.inner.finished.load(Ordering::SeqCst) {
            self.clear();
        }
    }
}

#[derive(Clone)]
pub struct LiveRow {
    inner: Arc<LiveRowInner>,
}

struct LiveRowInner {
    ui: Ui,
    presentation: LiveRowPresentation,
    state: Mutex<LiveRowState>,
    started: Instant,
    finished: AtomicBool,
}

struct LiveRowState {
    message: String,
    bar: Option<ProgressBar>,
}

enum LiveRowPresentation {
    Labeled(String),
    Rendered,
}

impl LiveRowPresentation {
    fn plain_message(&self, message: &str) -> String {
        match self {
            Self::Labeled(label) => format!("{label} · {message}"),
            Self::Rendered => message.to_owned(),
        }
    }
}

impl LiveRow {
    pub fn set_phase(&self, phase: impl Into<String>) {
        self.set_message(phase.into(), true);
    }

    pub fn set_rendered(&self, message: impl Into<String>) {
        self.set_message(message.into(), true);
    }

    fn set_message(&self, message: String, report_plain_change: bool) {
        let mut state = self.inner.state.lock().expect("live-row state lock");
        if state.message == message || self.inner.finished.load(Ordering::SeqCst) {
            return;
        }
        state.message.clone_from(&message);
        if let Some(bar) = state.bar.as_ref() {
            bar.set_message(message);
        } else if report_plain_change
            && matches!(
                self.inner.ui.inner.options.progress,
                ResolvedProgressMode::Plain
            )
        {
            self.inner.ui.write_line(&format!(
                "[phase] {}",
                self.inner.presentation.plain_message(&message)
            ));
        }
    }

    pub fn set_detail(&self, detail: impl Into<String>) {
        if self.inner.finished.load(Ordering::SeqCst) {
            return;
        }
        let detail = detail.into();
        let mut state = self.inner.state.lock().expect("live-row state lock");
        state.message.clone_from(&detail);
        if let Some(bar) = state.bar.as_ref() {
            bar.set_message(detail);
        }
    }

    pub fn finish(&self, message: impl Into<String>) {
        self.complete("ok", "✓", message.into());
    }

    pub fn finish_rendered(&self, message: impl Into<String>) {
        self.complete("ok", "✓", message.into());
    }

    pub fn fail(&self, message: impl Into<String>) {
        self.complete("fail", "✗", message.into());
    }

    pub fn abandon(&self, message: impl Into<String>) {
        self.complete("stop", "!", message.into());
    }

    pub fn clear(&self) {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(bar) = self
            .inner
            .state
            .lock()
            .expect("live-row state lock")
            .bar
            .take()
        {
            bar.finish_and_clear();
        }
    }

    fn complete(&self, plain_label: &str, glyph: &str, message: String) {
        if self.inner.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let elapsed = format_duration(self.inner.started.elapsed());
        if let Some(bar) = self
            .inner
            .state
            .lock()
            .expect("live-row state lock")
            .bar
            .take()
        {
            bar.disable_steady_tick();
            bar.set_style(message_style());
            bar.finish_with_message(match &self.inner.presentation {
                LiveRowPresentation::Labeled(label) => {
                    format!("{glyph} {label} · {message} · {elapsed}")
                }
                LiveRowPresentation::Rendered => format!("{glyph} {message} · {elapsed}"),
            });
        } else if matches!(
            self.inner.ui.inner.options.progress,
            ResolvedProgressMode::Plain
        ) {
            self.inner.ui.write_line(&format!(
                "[{plain_label}] {} · {elapsed}",
                self.inner.presentation.plain_message(&message)
            ));
        }
    }
}

impl Drop for LiveRow {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 && !self.inner.finished.load(Ordering::SeqCst) {
            self.clear();
        }
    }
}

fn live_footer_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {msg}"
    } else {
        "{spinner} {msg}"
    })
    .expect("live-group footer template")
    .tick_strings(SPINNER_TICKS)
}

fn live_row_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {prefix:<24} {msg}"
    } else {
        "{spinner} {prefix:<24} {msg}"
    })
    .expect("live-row template")
    .tick_strings(SPINNER_TICKS)
}

fn live_rendered_row_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {msg}"
    } else {
        "{spinner} {msg}"
    })
    .expect("rendered live-row template")
    .tick_strings(SPINNER_TICKS)
}

fn message_style() -> ProgressStyle {
    ProgressStyle::with_template("{msg}").expect("task message template")
}

pub fn format_duration(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        return format!("{}ms", duration.as_millis());
    }
    format_seconds(duration.as_secs())
}

fn format_seconds(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct BufferOutput {
        lines: Mutex<Vec<String>>,
    }

    impl LineOutput for BufferOutput {
        fn write_line(&self, line: &str) {
            self.lines
                .lock()
                .expect("buffer output lock")
                .push(line.to_owned());
        }
    }

    fn plain_ui(output: Arc<BufferOutput>, delay: Duration, heartbeat: Duration) -> Ui {
        Ui::with_output(
            UiOptions {
                progress: ProgressMode::Plain,
                color: ColorMode::Never,
                cancellation: CancellationMode::Passive,
                visibility_delay: delay,
                heartbeat_interval: heartbeat,
            }
            .validate_for(TerminalCapabilities {
                stderr: false,
                stdout: false,
            })
            .expect("plain UI options"),
            output,
        )
    }

    #[test]
    fn fast_clear_task_emits_nothing() {
        let output = Arc::new(BufferOutput::default());
        let ui = plain_ui(
            Arc::clone(&output),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        ui.task(TaskOptions {
            label: "Fast query".to_string(),
            ..TaskOptions::default()
        })
        .expect("task")
        .finish_and_clear();
        assert!(output.lines.lock().expect("lines").is_empty());
    }

    #[test]
    fn plain_task_reports_phases_and_completion() {
        let output = Arc::new(BufferOutput::default());
        let ui = plain_ui(Arc::clone(&output), Duration::ZERO, Duration::from_secs(1));
        let task = ui
            .task(TaskOptions {
                label: "Enroll host".to_string(),
                visibility: TaskVisibility::Immediate,
                ..TaskOptions::default()
            })
            .expect("task");
        while output.lines.lock().expect("lines").is_empty() {
            thread::yield_now();
        }
        task.set_phase("activating");
        task.finish("Host enrolled");
        let lines = output.lines.lock().expect("lines");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[start] Enroll host · elapsed "))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[phase] Enroll host · activating"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[ok] Host enrolled"))
        );
    }

    #[test]
    fn task_options_reject_empty_labels_and_zero_totals() {
        assert!(TaskOptions::default().validate().is_err());
        assert!(
            TaskOptions {
                label: "count".to_string(),
                kind: TaskKind::Counter {
                    total: 0,
                    unit: None,
                },
                ..TaskOptions::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn forced_interactive_requires_a_terminal() {
        assert!(
            UiOptions {
                progress: ProgressMode::Interactive,
                ..UiOptions::default()
            }
            .validate_for(TerminalCapabilities {
                stderr: false,
                stdout: false,
            })
            .is_err()
        );
    }

    #[test]
    fn duration_format_is_compact_and_stable() {
        assert_eq!("25ms", format_duration(Duration::from_millis(25)));
        assert_eq!("1:05", format_duration(Duration::from_secs(65)));
        assert_eq!("2:03:04", format_duration(Duration::from_secs(7_384)));
    }

    #[test]
    fn plain_live_group_reports_phase_and_outcomes() {
        let output = Arc::new(BufferOutput::default());
        let ui = plain_ui(Arc::clone(&output), Duration::ZERO, Duration::from_secs(1));
        let group = ui.live_group("Redeploying fleet").expect("live group");
        let row = group.row("coeus", "queued").expect("live row");
        row.set_phase("installing");
        row.finish("redeployed");
        group.finish("Fleet redeploy complete");

        let lines = output.lines.lock().expect("lines");
        assert!(lines.iter().any(|line| line == "[start] Redeploying fleet"));
        assert!(lines.iter().any(|line| line == "[item] coeus · queued"));
        assert!(
            lines
                .iter()
                .any(|line| line == "[phase] coeus · installing")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[ok] coeus · redeployed"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[ok] Fleet redeploy complete"))
        );
    }

    #[test]
    fn rendered_live_rows_preserve_caller_layout_and_silent_updates() {
        let output = Arc::new(BufferOutput::default());
        let ui = plain_ui(Arc::clone(&output), Duration::ZERO, Duration::from_secs(1));
        let group = ui.live_group("Checking hosts").expect("live group");
        let row = group.rendered_row("alpha    queued").expect("rendered row");
        row.set_detail("alpha    probing");
        row.set_rendered("alpha    reachable");
        row.finish_rendered("alpha    available");
        group.clear();

        let lines = output.lines.lock().expect("lines");
        assert!(lines.iter().any(|line| line == "[item] alpha    queued"));
        assert!(!lines.iter().any(|line| line.contains("probing")));
        assert!(
            lines
                .iter()
                .any(|line| line == "[phase] alpha    reachable")
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[ok] alpha    available"))
        );
    }

    #[test]
    fn explicit_color_mode_applies_to_stderr_and_stdout_renderers() {
        for (color, expected) in [(ColorMode::Always, true), (ColorMode::Never, false)] {
            let options = UiOptions {
                color,
                ..UiOptions::default()
            }
            .validate_for(TerminalCapabilities {
                stderr: false,
                stdout: true,
            })
            .expect("UI options");
            assert_eq!(expected, options.color);
            assert_eq!(expected, options.stdout_color);
        }
    }
}
