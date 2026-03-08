use std::path::PathBuf;

use anyhow::{Result, anyhow};

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("Failed to determine home directory"))
}

pub fn app_dir(tool: &str) -> Result<PathBuf> {
    Ok(home_dir()?.join(format!(".{}", sanitize_component(tool))))
}

fn sanitize_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push('-');
        }
    }
    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        "tool".to_owned()
    } else {
        trimmed.to_owned()
    }
}
