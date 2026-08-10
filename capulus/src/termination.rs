use std::process::{ExitCode, Termination};

use anyhow::Result;

use crate::{cancel::INTERRUPTED_EXIT_CODE, error_is_cancelled, progress::Ui};

pub struct CliTermination {
    result: Result<i32>,
    ui: Option<Ui>,
}

impl CliTermination {
    pub fn without_ui(result: Result<i32>) -> Self {
        Self { result, ui: None }
    }

    pub fn with_ui(ui: &Ui, result: Result<i32>) -> Self {
        Self {
            result,
            ui: Some(ui.clone()),
        }
    }

    fn report_error(ui: Option<&Ui>, message: &str) {
        match ui {
            Some(ui) => ui.error(message),
            None => crate::ui::print_error(message),
        }
    }
}

impl Termination for CliTermination {
    fn report(self) -> ExitCode {
        let Self { result, ui } = self;
        match result {
            Ok(code) => match u8::try_from(code) {
                Ok(code) => ExitCode::from(code),
                Err(_) => {
                    Self::report_error(ui.as_ref(), &format!("invalid command exit code {code}"));
                    ExitCode::FAILURE
                }
            },
            Err(error) if error_is_cancelled(&error) => ExitCode::from(
                u8::try_from(INTERRUPTED_EXIT_CODE).expect("SIGINT exit code fits in one byte"),
            ),
            Err(error) => {
                Self::report_error(ui.as_ref(), &error.to_string());
                ExitCode::FAILURE
            }
        }
    }
}
