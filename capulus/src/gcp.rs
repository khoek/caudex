use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::paths::home_dir;
use crate::process::run_json;
use crate::store::write_toml_file;

const DEFAULT_ACCESS_TOKEN_LIFETIME_MS: u128 = 60 * 60 * 1_000;
const ACCESS_TOKEN_REFRESH_SKEW_MS: u128 = 60 * 1_000;

#[derive(Debug, Clone, Copy)]
pub struct AccessTokenRequest<'a> {
    pub configured_credentials_path: Option<&'a str>,
    pub cache_path: &'a Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAccessToken {
    token: String,
    expires_at_epoch_ms: u128,
    credential_source: String,
}

#[derive(Debug, Deserialize)]
struct AccessTokenJson {
    token: String,
    #[serde(default, alias = "tokenExpiry")]
    token_expiry: Option<String>,
}

#[derive(Debug, Clone)]
enum CredentialSource {
    ActiveAccount(String),
    ApplicationDefault(String),
}

impl CredentialSource {
    fn key(&self) -> String {
        match self {
            Self::ActiveAccount(account) => format!("account:{account}"),
            Self::ApplicationDefault(path) => format!("adc:{path}"),
        }
    }

    fn command_args(&self) -> [&'static str; 4] {
        match self {
            Self::ActiveAccount(_) => ["auth", "print-access-token", "--format=json", ""],
            Self::ApplicationDefault(_) => [
                "auth",
                "application-default",
                "print-access-token",
                "--format=json",
            ],
        }
    }
}

pub fn detect_project(cached_project: Option<&str>, include_cached: bool) -> Option<String> {
    if include_cached && let Some(project) = nonempty(cached_project) {
        return Some(project.to_owned());
    }
    for env_key in [
        "CLOUDSDK_CORE_PROJECT",
        "GOOGLE_CLOUD_PROJECT",
        "GCLOUD_PROJECT",
    ] {
        if let Ok(value) = std::env::var(env_key)
            && let Some(project) = nonempty(Some(value.as_str()))
        {
            return Some(project.to_owned());
        }
    }

    let mut command = Command::new("gcloud");
    command.args(["config", "get-value", "project", "--quiet"]);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("(unset)") {
        None
    } else {
        Some(value.to_owned())
    }
}

pub fn detect_credentials_path(cached_path: Option<&str>, include_cached: bool) -> Option<String> {
    let mut candidates = Vec::new();
    if include_cached && let Some(path) = nonempty(cached_path) {
        candidates.push(path.to_owned());
    }
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        && let Some(path) = nonempty(Some(path.as_str()))
    {
        candidates.push(path.to_owned());
    }
    if let Ok(home) = home_dir() {
        candidates.push(
            home.join(".config/gcloud/application_default_credentials.json")
                .display()
                .to_string(),
        );
    }

    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

pub fn active_account() -> Result<Option<String>> {
    let mut command = Command::new("gcloud");
    command.args([
        "auth",
        "list",
        "--filter=status:ACTIVE",
        "--format=value(account)",
    ]);
    let output = command
        .output()
        .context("Failed to run `gcloud auth list` for credential detection")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned))
}

pub fn has_active_account() -> Result<bool> {
    active_account().map(|account| account.is_some())
}

pub fn command(configured_credentials_path: Option<&str>) -> Command {
    let mut command = Command::new("gcloud");
    if let Some(path) = nonempty(configured_credentials_path) {
        command.env("GOOGLE_APPLICATION_CREDENTIALS", path);
    }
    command
}

pub fn access_token(request: AccessTokenRequest<'_>) -> Result<String> {
    let source = credential_source(request.configured_credentials_path)?;
    if let Some(token) = load_cached_access_token(request.cache_path, &source)? {
        return Ok(token);
    }

    let mut command = command(request.configured_credentials_path);
    let args = source.command_args();
    if matches!(source, CredentialSource::ActiveAccount(_)) {
        command.args(&args[..3]);
    } else {
        command.args(args);
    }
    let response = run_json::<AccessTokenJson>(&mut command, "obtain a GCP access token")?;
    let token = response.token.trim();
    if token.is_empty() {
        bail!("GCP access token lookup returned an empty token.");
    }
    save_cached_access_token(
        request.cache_path,
        &source,
        token,
        expires_at_epoch_ms(response.token_expiry.as_deref())?,
    )?;
    Ok(token.to_owned())
}

fn credential_source(configured_credentials_path: Option<&str>) -> Result<CredentialSource> {
    if let Some(path) = nonempty(configured_credentials_path) {
        return Ok(CredentialSource::ApplicationDefault(path.to_owned()));
    }
    if let Some(account) = active_account()? {
        return Ok(CredentialSource::ActiveAccount(account));
    }
    if let Some(path) = detect_credentials_path(None, false) {
        return Ok(CredentialSource::ApplicationDefault(path));
    }
    bail!("No active gcloud account or application-default credentials were found.")
}

fn load_cached_access_token(
    cache_path: &Path,
    source: &CredentialSource,
) -> Result<Option<String>> {
    let content = match fs::read_to_string(cache_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to read cached GCP access token: {}",
                    cache_path.display()
                )
            });
        }
    };
    if content.trim().is_empty() {
        return Ok(None);
    }
    let Ok(cached) = toml::from_str::<CachedAccessToken>(&content) else {
        return Ok(None);
    };
    if cached.token.trim().is_empty()
        || cached.credential_source != source.key()
        || now_epoch_ms().saturating_add(ACCESS_TOKEN_REFRESH_SKEW_MS) >= cached.expires_at_epoch_ms
    {
        return Ok(None);
    }
    Ok(Some(cached.token))
}

fn save_cached_access_token(
    cache_path: &Path,
    source: &CredentialSource,
    token: &str,
    expires_at_epoch_ms: u128,
) -> Result<()> {
    write_toml_file(
        cache_path,
        &CachedAccessToken {
            token: token.to_owned(),
            expires_at_epoch_ms,
            credential_source: source.key(),
        },
        Some(0o600),
        Some(0o700),
    )
    .with_context(|| {
        format!(
            "Failed to write cached GCP access token: {}",
            cache_path.display()
        )
    })
}

fn expires_at_epoch_ms(token_expiry: Option<&str>) -> Result<u128> {
    if let Some(token_expiry) = token_expiry
        && let Ok(expiry) = OffsetDateTime::parse(token_expiry.trim(), &Rfc3339)
    {
        return Ok(expiry.unix_timestamp_nanos() as u128 / 1_000_000);
    }
    Ok(now_epoch_ms().saturating_add(DEFAULT_ACCESS_TOKEN_LIFETIME_MS))
}

fn now_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::expires_at_epoch_ms;

    #[test]
    fn invalid_expiry_uses_default_lifetime() {
        let now = expires_at_epoch_ms(Some("not-a-date")).expect("default lifetime should work");
        assert!(now > 0);
    }
}
