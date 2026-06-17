use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rand::RngExt;
use serde::{Deserialize, Serialize};

use crate::paths::app_dir;
use crate::store::{ensure_directory, write_toml_file};

const CONTAINERS_DIR_NAME: &str = "containers";
const METADATA_FILE_NAME: &str = "metadata.toml";
const IMAGE_ARCHIVE_FILE_NAME: &str = "image.tar";
const LABEL_DOMAIN_PREFIX: &str = "io.khoek.";
const ARTIFACT_ID_LENGTH: usize = 8;
const ARTIFACT_ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub schema_version: u32,
    pub artifact_id: String,
    pub remote_tag: String,
    pub build_fingerprint: String,
    pub kind: String,
    pub created_at_epoch_ms: u128,
    pub crate_name: String,
    pub crate_version: String,
    pub binary_name: String,
    pub source_path: String,
    pub manifest_path: String,
    pub cargo_profile: String,
    #[serde(default)]
    pub cargo_features: Vec<String>,
    pub base_image: String,
    pub runtime: String,
    pub local_tag: String,
    pub archive_file: String,
    pub uploaded_ref: Option<String>,
    pub uploaded_at_epoch_ms: Option<u128>,
}

#[derive(Debug, Clone)]
pub struct StoredArtifact {
    pub dir: PathBuf,
    pub metadata: ArtifactMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSummary {
    pub artifact_id: String,
    pub remote_tag: String,
    pub build_fingerprint: String,
    pub created_at_epoch_ms: u128,
    pub crate_name: String,
    pub binary_name: String,
    pub cargo_profile: String,
    pub cargo_features: Vec<String>,
    pub base_image: String,
}

impl StoredArtifact {
    pub fn archive_path(&self) -> PathBuf {
        self.dir.join(&self.metadata.archive_file)
    }
}

impl From<&ArtifactMetadata> for ArtifactSummary {
    fn from(metadata: &ArtifactMetadata) -> Self {
        Self {
            artifact_id: metadata.artifact_id.clone(),
            remote_tag: metadata.remote_tag.clone(),
            build_fingerprint: metadata.build_fingerprint.clone(),
            created_at_epoch_ms: metadata.created_at_epoch_ms,
            crate_name: metadata.crate_name.clone(),
            binary_name: metadata.binary_name.clone(),
            cargo_profile: metadata.cargo_profile.clone(),
            cargo_features: metadata.cargo_features.clone(),
            base_image: metadata.base_image.clone(),
        }
    }
}

pub fn containers_root(root: impl AsRef<Path>, tool: &str) -> PathBuf {
    app_dir(root, tool).join(CONTAINERS_DIR_NAME)
}

pub fn ensure_containers_root(root: impl AsRef<Path>, tool: &str) -> Result<PathBuf> {
    let path = containers_root(root, tool);
    ensure_directory(&path, None)?;
    Ok(path)
}

pub fn create_artifact_dir(root: impl AsRef<Path>, tool: &str) -> Result<(String, PathBuf)> {
    let root = ensure_containers_root(root, tool)?;
    for _ in 0..128 {
        let artifact_id = random_artifact_id();
        let dir = root.join(&artifact_id);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((artifact_id, dir)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create artifact directory: {}", dir.display())
                });
            }
        }
    }
    Err(anyhow!(
        "Failed to allocate a unique 8-character artifact id after repeated attempts."
    ))
}

pub fn default_archive_file_name() -> &'static str {
    IMAGE_ARCHIVE_FILE_NAME
}

pub fn save_metadata(dir: &Path, metadata: &ArtifactMetadata) -> Result<()> {
    write_toml_file(&dir.join(METADATA_FILE_NAME), metadata, None, None)
        .with_context(|| format!("Failed to write artifact metadata in {}", dir.display()))
}

pub fn load_stored_artifacts(root: impl AsRef<Path>, tool: &str) -> Result<Vec<StoredArtifact>> {
    let root = ensure_containers_root(root, tool)?;
    let mut artifacts = Vec::new();
    for entry in fs::read_dir(&root)
        .with_context(|| format!("Failed to read container directory: {}", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let metadata_path = path.join(METADATA_FILE_NAME);
        if !metadata_path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&metadata_path).with_context(|| {
            format!(
                "Failed to read artifact metadata: {}",
                metadata_path.display()
            )
        })?;
        let Ok(metadata) = toml::from_str::<ArtifactMetadata>(&content) else {
            continue;
        };
        if metadata.schema_version != CURRENT_SCHEMA_VERSION {
            continue;
        }
        artifacts.push(StoredArtifact {
            dir: path,
            metadata,
        });
    }
    artifacts.sort_by(|left, right| {
        right
            .metadata
            .created_at_epoch_ms
            .cmp(&left.metadata.created_at_epoch_ms)
    });
    Ok(artifacts)
}

pub fn resolve_artifact(
    root: impl AsRef<Path>,
    tool: &str,
    selector: Option<&str>,
) -> Result<StoredArtifact> {
    let root = root.as_ref();
    let artifacts = load_stored_artifacts(root, tool)?;
    if artifacts.is_empty() {
        bail!(
            "No local artifacts found in `{}`.",
            containers_root(root, tool).display()
        );
    }
    let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(artifacts[0].clone());
    };

    if let Some(found) = artifacts
        .iter()
        .find(|artifact| artifact.metadata.artifact_id == selector)
    {
        return Ok(found.clone());
    }

    let id_prefix_matches: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.metadata.artifact_id.starts_with(selector))
        .cloned()
        .collect();
    if id_prefix_matches.len() == 1 {
        return Ok(id_prefix_matches[0].clone());
    }
    if id_prefix_matches.len() > 1 {
        let ids = id_prefix_matches
            .iter()
            .map(|artifact| artifact.metadata.artifact_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Artifact selector `{selector}` is ambiguous: {ids}");
    }

    let crate_matches: Vec<_> = artifacts
        .iter()
        .filter(|artifact| artifact.metadata.crate_name == selector)
        .cloned()
        .collect();
    if crate_matches.len() == 1 {
        return Ok(crate_matches[0].clone());
    }
    if crate_matches.len() > 1 {
        let ids = crate_matches
            .iter()
            .map(|artifact| artifact.metadata.artifact_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Crate selector `{selector}` matches multiple artifacts: {ids}");
    }

    Err(anyhow!("No artifact matched `{selector}`."))
}

pub fn sanitize_segment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_dash = false;
    for ch in value.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if mapped == '-' {
            if !last_dash && !output.is_empty() {
                output.push('-');
            }
            last_dash = true;
        } else {
            output.push(mapped);
            last_dash = false;
        }
    }
    output.trim_matches('-').to_owned()
}

pub fn remote_tag(crate_name: &str, artifact_id: &str) -> String {
    format!("{}-{artifact_id}", sanitize_segment(crate_name))
}

pub fn tracking_tag(artifact_id: &str) -> String {
    artifact_id.to_owned()
}

pub fn artifact_id_from_tracking_tag(tag: &str) -> Option<&str> {
    is_valid_artifact_id(tag).then_some(tag)
}

pub fn image_labels(tool: &str, metadata: &ArtifactMetadata) -> Vec<(String, String)> {
    vec![
        (
            label_key(tool, "schema-version"),
            metadata.schema_version.to_string(),
        ),
        (label_key(tool, "artifact-id"), metadata.artifact_id.clone()),
        (label_key(tool, "remote-tag"), metadata.remote_tag.clone()),
        (
            label_key(tool, "build-fingerprint"),
            metadata.build_fingerprint.clone(),
        ),
        (
            label_key(tool, "created-at-epoch-ms"),
            metadata.created_at_epoch_ms.to_string(),
        ),
        (label_key(tool, "crate-name"), metadata.crate_name.clone()),
        (label_key(tool, "binary-name"), metadata.binary_name.clone()),
        (
            label_key(tool, "cargo-profile"),
            metadata.cargo_profile.clone(),
        ),
        (
            label_key(tool, "cargo-features"),
            metadata.cargo_features.join(","),
        ),
        (label_key(tool, "base-image"), metadata.base_image.clone()),
    ]
}

pub fn artifact_summary_from_labels(
    tool: &str,
    labels: &HashMap<String, String>,
) -> Option<ArtifactSummary> {
    let schema_version = label_value(tool, labels, "schema-version")?
        .parse::<u32>()
        .ok()?;
    if schema_version != CURRENT_SCHEMA_VERSION {
        return None;
    }
    Some(ArtifactSummary {
        artifact_id: label_value(tool, labels, "artifact-id")?,
        remote_tag: label_value(tool, labels, "remote-tag")?,
        build_fingerprint: label_value(tool, labels, "build-fingerprint")?,
        created_at_epoch_ms: label_value(tool, labels, "created-at-epoch-ms")?
            .parse()
            .ok()?,
        crate_name: label_value(tool, labels, "crate-name")?,
        binary_name: label_value(tool, labels, "binary-name")?,
        cargo_profile: label_value(tool, labels, "cargo-profile")?,
        cargo_features: parse_label_features(label_value(tool, labels, "cargo-features")?.as_str()),
        base_image: label_value(tool, labels, "base-image")?,
    })
}

fn random_artifact_id() -> String {
    let mut rng = rand::rng();
    (0..ARTIFACT_ID_LENGTH)
        .map(|_| ARTIFACT_ID_ALPHABET[rng.random_range(0..ARTIFACT_ID_ALPHABET.len())] as char)
        .collect()
}

fn is_valid_artifact_id(value: &str) -> bool {
    value.len() == ARTIFACT_ID_LENGTH
        && value
            .bytes()
            .all(|byte| ARTIFACT_ID_ALPHABET.contains(&byte))
}

fn label_key(tool: &str, suffix: &str) -> String {
    format!("{}{suffix}", label_prefix(tool))
}

fn label_prefix(tool: &str) -> String {
    let component = sanitize_segment(tool);
    if component.is_empty() {
        format!("{LABEL_DOMAIN_PREFIX}tool.")
    } else {
        format!("{LABEL_DOMAIN_PREFIX}{component}.")
    }
}

fn label_value(tool: &str, labels: &HashMap<String, String>, suffix: &str) -> Option<String> {
    labels.get(&label_key(tool, suffix)).cloned()
}

fn parse_label_features(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{ArtifactMetadata, artifact_summary_from_labels, containers_root, image_labels};

    fn metadata() -> ArtifactMetadata {
        ArtifactMetadata {
            schema_version: super::CURRENT_SCHEMA_VERSION,
            artifact_id: "abc123xy".to_owned(),
            remote_tag: "demo-abc123xy".to_owned(),
            build_fingerprint: "fingerprint".to_owned(),
            kind: "container".to_owned(),
            created_at_epoch_ms: 1234,
            crate_name: "demo".to_owned(),
            crate_version: "1.2.3".to_owned(),
            binary_name: "demo".to_owned(),
            source_path: "/src".to_owned(),
            manifest_path: "/src/Cargo.toml".to_owned(),
            cargo_profile: "release".to_owned(),
            cargo_features: vec!["gpu".to_owned(), "cloud".to_owned()],
            base_image: "debian:bookworm".to_owned(),
            runtime: "docker".to_owned(),
            local_tag: "local".to_owned(),
            archive_file: "image.tar".to_owned(),
            uploaded_ref: None,
            uploaded_at_epoch_ms: None,
        }
    }

    #[test]
    fn containers_root_uses_sanitized_tool_namespace() {
        assert_eq!(
            PathBuf::from("state").join("fancy-tool").join("containers"),
            containers_root("state", "Fancy Tool!")
        );
    }

    #[test]
    fn image_labels_are_scoped_to_the_tool_name() {
        let labels: HashMap<_, _> = image_labels("Fancy Tool!", &metadata())
            .into_iter()
            .collect();

        assert_eq!(
            Some(&super::CURRENT_SCHEMA_VERSION.to_string()),
            labels.get("io.khoek.fancy-tool.schema-version")
        );
        assert!(labels.contains_key("io.khoek.fancy-tool.artifact-id"));
        assert!(!labels.contains_key("io.khoek.tool.artifact-id"));
    }

    #[test]
    fn artifact_summary_reads_only_matching_tool_labels() {
        let labels: HashMap<_, _> = image_labels("Fancy Tool!", &metadata())
            .into_iter()
            .collect();

        let summary = artifact_summary_from_labels("Fancy Tool!", &labels)
            .expect("matching tool labels should produce a summary");
        assert_eq!("abc123xy", summary.artifact_id);
        assert_eq!(
            vec!["gpu".to_owned(), "cloud".to_owned()],
            summary.cargo_features
        );
        assert!(artifact_summary_from_labels("Other Tool", &labels).is_none());
    }
}
