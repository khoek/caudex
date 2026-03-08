use std::io::{self, IsTerminal};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::Confirm;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::{Deserialize, Serialize};

const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
const ANSI_BOLD_GREEN: &str = "\x1b[1;32m";
const ANSI_BOLD_RED: &str = "\x1b[1;31m";
const ANSI_BOLD_WHITE_RED_BG: &str = "\x1b[1;37;41m";
const ANSI_BOLD_YELLOW: &str = "\x1b[1;33m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_STRIKETHROUGH: &str = "\x1b[9m";
const ANSI_YELLOW: &str = "\x1b[33m";
const SPINNER_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    Blue,
    Cyan,
    Green,
    Red,
    Yellow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TextEffect {
    #[default]
    None,
    Strikethrough,
}

pub trait RenderTarget {
    fn style(&self, text: &str, color: Option<Color>, effect: TextEffect) -> String;

    fn paint(&self, text: &str, color: Color) -> String {
        self.style(text, Some(color), TextEffect::None)
    }

    fn effect(&self, text: &str, effect: TextEffect) -> String {
        self.style(text, None, effect)
    }

    fn hyperlink(&self, text: &str, url: Option<&str>) -> String {
        let _ = url;
        text.to_owned()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdoutRenderTarget;

#[derive(Debug, Clone, Copy, Default)]
pub struct StderrRenderTarget;

pub fn stdin_is_interactive() -> bool {
    io::stdin().is_terminal()
}

pub fn stdout_is_interactive() -> bool {
    io::stdout().is_terminal()
}

pub fn stderr_is_interactive() -> bool {
    io::stderr().is_terminal()
}

pub fn require_interactive(message: &str) -> Result<()> {
    if stdin_is_interactive() {
        Ok(())
    } else {
        bail!("{message}")
    }
}

pub fn prompt_confirm(prompt: &str, default: bool) -> Result<bool> {
    prompt_confirm_with_message(prompt, default, "Interactive confirmation required.")
}

pub fn prompt_confirm_with_message(prompt: &str, default: bool, message: &str) -> Result<bool> {
    require_interactive(message)?;
    Confirm::with_theme(prompt_theme())
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(Into::into)
}

pub fn prompt_theme() -> &'static ColorfulTheme {
    static THEME: LazyLock<ColorfulTheme> = LazyLock::new(ColorfulTheme::default);
    &THEME
}

pub fn stdout_render_target() -> StdoutRenderTarget {
    StdoutRenderTarget
}

pub fn stderr_render_target() -> StderrRenderTarget {
    StderrRenderTarget
}

pub fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(SPINNER_TICKS)
}

pub fn plain_message_style() -> ProgressStyle {
    ProgressStyle::with_template("{msg}").unwrap_or_else(|_| ProgressStyle::default_spinner())
}

pub fn spinner(message: &str) -> ProgressBar {
    let progress = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
    progress.set_style(spinner_style("{spinner:.cyan} {msg}"));
    progress.enable_steady_tick(Duration::from_millis(90));
    progress.set_message(message.to_owned());
    progress
}

pub fn new_list_row_spinner() -> ProgressBar {
    let row = ProgressBar::new_spinner();
    row.set_style(spinner_style("{spinner:.cyan} {msg}"));
    row
}

pub fn new_list_spacer() -> ProgressBar {
    let spacer = ProgressBar::new_spinner();
    spacer.set_style(plain_message_style());
    spacer
}

pub fn activate_list_row_spinner(row: &ProgressBar) {
    row.enable_steady_tick(Duration::from_millis(90));
}

pub fn finish_list_spacer(spacer: &ProgressBar) {
    spacer.finish_with_message(" ".to_owned());
}

pub fn finish_list_row(row: &ProgressBar, message: &str) {
    row.set_style(plain_message_style());
    row.finish_with_message(message.to_owned());
}

pub fn clear_list_spacer(progress: &MultiProgress, spacer: &ProgressBar) {
    progress.remove(spacer);
}

pub fn clear_progress_bar(progress: &MultiProgress, progress_bar: &ProgressBar) {
    progress_bar.finish_and_clear();
    progress.remove(progress_bar);
}

pub fn maybe_open_browser(url: &str) {
    if let Err(err) = webbrowser::open(url) {
        warn(&format!(
            "Could not open a browser automatically: {err}. Open this URL manually: {url}"
        ));
    }
}

pub fn print_big_red_error(message: &str) {
    if stderr_is_interactive() {
        eprintln!(
            "{} ERROR {} {}{}{}",
            ANSI_BOLD_WHITE_RED_BG, ANSI_RESET, ANSI_BOLD_RED, message, ANSI_RESET
        );
    } else {
        eprintln!("ERROR: {message}");
    }
}

pub fn print_notice(message: &str) {
    if stderr_is_interactive() {
        let prefix = StderrRenderTarget.paint("INFO", Color::Cyan);
        eprintln!("{prefix} {message}");
    } else {
        eprintln!("INFO: {message}");
    }
}

pub fn print_warning(message: &str) {
    warn(message);
}

pub fn print_error(message: &str) {
    if stderr_is_interactive() {
        eprintln!("{ANSI_BOLD_RED}error:{ANSI_RESET} {message}");
    } else {
        eprintln!("error: {message}");
    }
}

pub fn print_stage(message: &str) {
    stage(message);
}

pub fn stage(message: &str) {
    if stderr_is_interactive() {
        eprintln!("{ANSI_BOLD_CYAN}==>{ANSI_RESET} {message}");
    } else {
        eprintln!("==> {message}");
    }
}

pub fn detail(message: &str) {
    eprintln!("    {message}");
}

pub fn success(message: &str) {
    if stderr_is_interactive() {
        eprintln!("{ANSI_BOLD_GREEN}ok:{ANSI_RESET} {message}");
    } else {
        eprintln!("ok: {message}");
    }
}

pub fn warn(message: &str) {
    if stderr_is_interactive() {
        eprintln!("{ANSI_BOLD_YELLOW}warning:{ANSI_RESET} {message}");
    } else {
        eprintln!("warning: {message}");
    }
}

impl RenderTarget for StdoutRenderTarget {
    fn style(&self, text: &str, color: Option<Color>, effect: TextEffect) -> String {
        style_for_terminal(text, color, effect, stdout_is_interactive())
    }

    fn hyperlink(&self, text: &str, url: Option<&str>) -> String {
        if !stdout_is_interactive() {
            return text.to_owned();
        }
        let Some(url) = url else {
            return text.to_owned();
        };
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    }
}

impl RenderTarget for StderrRenderTarget {
    fn style(&self, text: &str, color: Option<Color>, effect: TextEffect) -> String {
        style_for_terminal(text, color, effect, stderr_is_interactive())
    }
}

fn ansi_code(color: Color) -> &'static str {
    match color {
        Color::Blue => ANSI_BLUE,
        Color::Cyan => ANSI_CYAN,
        Color::Green => ANSI_GREEN,
        Color::Red => ANSI_RED,
        Color::Yellow => ANSI_YELLOW,
    }
}

fn style_for_terminal(
    text: &str,
    color: Option<Color>,
    effect: TextEffect,
    is_interactive: bool,
) -> String {
    if !is_interactive || (color.is_none() && effect == TextEffect::None) {
        return text.to_owned();
    }
    let mut prefix = String::new();
    if effect == TextEffect::Strikethrough {
        prefix.push_str(ANSI_STRIKETHROUGH);
    }
    if let Some(color) = color {
        prefix.push_str(ansi_code(color));
    }
    format!("{prefix}{text}{ANSI_RESET}")
}
