use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", deny_unknown_fields)]
pub enum CargoRegistry {
    CratesIo,
    Private {
        name: String,
        index: String,
        token: String,
        ca_pem: String,
    },
}

impl std::fmt::Debug for CargoRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CratesIo => formatter.write_str("CratesIo"),
            Self::Private { name, index, .. } => formatter
                .debug_struct("Private")
                .field("name", name)
                .field("index", index)
                .field("token", &"[REDACTED]")
                .field("ca_pem", &"[REDACTED]")
                .finish(),
        }
    }
}

impl CargoRegistry {
    pub fn private(
        name: impl Into<String>,
        index: impl Into<String>,
        token: impl Into<String>,
        ca_pem: impl Into<String>,
    ) -> Result<Self, ReleaseValidationError> {
        let registry = Self::Private {
            name: name.into(),
            index: index.into(),
            token: token.into(),
            ca_pem: ca_pem.into(),
        };
        registry.validate()?;
        Ok(registry)
    }

    pub fn validate(&self) -> Result<(), ReleaseValidationError> {
        let Self::Private {
            name,
            index,
            token,
            ca_pem,
        } = self
        else {
            return Ok(());
        };
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(ReleaseValidationError::RegistryName);
        }
        let url = index.strip_prefix("sparse+").unwrap_or(index);
        if !url.starts_with("https://") || url.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(ReleaseValidationError::RegistryIndex);
        }
        if token.trim().is_empty() || token.len() > 16 * 1024 || token.bytes().any(|b| b == 0) {
            return Err(ReleaseValidationError::RegistryToken);
        }
        if ca_pem.len() > 1024 * 1024
            || !ca_pem.contains("-----BEGIN CERTIFICATE-----")
            || !ca_pem.contains("-----END CERTIFICATE-----")
        {
            return Err(ReleaseValidationError::RegistryCa);
        }
        Ok(())
    }

    pub(crate) fn cargo_registry_name(&self) -> Option<&str> {
        match self {
            Self::CratesIo => None,
            Self::Private { name, .. } => Some(name),
        }
    }

    pub(crate) fn configuration(
        &self,
        ca_path: &Path,
    ) -> Result<CargoConfiguration<'_>, toml::ser::Error> {
        let mut config = toml::Table::from_iter([(
            "net".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "retry".to_string(),
                toml::Value::Integer(2),
            )])),
        )]);
        let Self::Private {
            name,
            index,
            token,
            ca_pem,
        } = self
        else {
            return Ok(CargoConfiguration {
                config: toml::to_string(&config)?,
                credentials: None,
                ca_pem: None,
            });
        };
        config.insert(
            "registries".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                name.clone(),
                toml::Value::Table(toml::Table::from_iter([
                    ("index".to_string(), toml::Value::String(index.clone())),
                    (
                        "credential-provider".to_string(),
                        toml::Value::String("cargo:token".to_string()),
                    ),
                ])),
            )])),
        );
        config.insert(
            "registry".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "global-credential-providers".to_string(),
                toml::Value::Array(vec![toml::Value::String("cargo:token".to_string())]),
            )])),
        );
        config.insert(
            "http".to_string(),
            toml::Value::Table(toml::Table::from_iter([(
                "cainfo".to_string(),
                toml::Value::String(ca_path.display().to_string()),
            )])),
        );
        Ok(CargoConfiguration {
            config: toml::to_string(&config)?,
            credentials: Some(toml::to_string(&toml::Table::from_iter([(
                "registries".to_string(),
                toml::Value::Table(toml::Table::from_iter([(
                    name.clone(),
                    toml::Value::Table(toml::Table::from_iter([(
                        "token".to_string(),
                        toml::Value::String(token.clone()),
                    )])),
                )])),
            )]))?),
            ca_pem: Some(ca_pem),
        })
    }
}

pub(crate) struct CargoConfiguration<'a> {
    pub config: String,
    pub credentials: Option<String>,
    pub ca_pem: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRelease {
    pub version: Version,
    pub registry: CargoRegistry,
}

impl ResolvedRelease {
    pub fn validate(&self) -> Result<(), ReleaseValidationError> {
        if !self.version.pre.is_empty() || !self.version.build.is_empty() {
            return Err(ReleaseValidationError::StableVersion);
        }
        self.registry.validate()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReleaseValidationError {
    #[error("private Cargo registry name is invalid")]
    RegistryName,
    #[error("private Cargo registry index must use HTTPS")]
    RegistryIndex,
    #[error("private Cargo registry token is missing or invalid")]
    RegistryToken,
    #[error("private Cargo registry CA bundle is missing or invalid")]
    RegistryCa,
    #[error("managed releases must use a stable version without build metadata")]
    StableVersion,
}
