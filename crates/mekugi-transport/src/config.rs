use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_BOX_NAME: &str = "default";
pub const DEFAULT_MAX_YIELD_TIME_MS: u32 = 45_000;
pub const HARD_MAX_YIELD_TIME_MS: u32 = 300_000;
pub const DEFAULT_GRPC_POOL_SIZE: usize = 1;
pub const DEFAULT_HTTP_POOL_SIZE: usize = 16;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read boxes config file '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse boxes config JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid cluster configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportScheme {
    Grpc,
    Grpcs,
    Http,
    Https,
}

impl TransportScheme {
    pub fn parse_from_target(target: &str) -> Result<Self, ConfigError> {
        let lower = target.trim().to_ascii_lowercase();
        if lower.starts_with("grpcs://") {
            Ok(Self::Grpcs)
        } else if lower.starts_with("grpc://") {
            Ok(Self::Grpc)
        } else if lower.starts_with("https://") {
            Ok(Self::Https)
        } else if lower.starts_with("http://") {
            Ok(Self::Http)
        } else {
            Err(ConfigError::Invalid(format!(
                "target '{target}' must start with grpc://, grpcs://, http://, or https://"
            )))
        }
    }

    pub fn is_grpc(self) -> bool {
        matches!(self, Self::Grpc | Self::Grpcs)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    pub name: String,
    #[serde(alias = "url")]
    pub target: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub pool_size: Option<usize>,
    #[serde(default)]
    pub max_yield_time_ms: Option<u32>,
    #[serde(default)]
    pub ca_cert_path: Option<String>,
}

impl BoxConfig {
    pub fn scheme(&self) -> Result<TransportScheme, ConfigError> {
        TransportScheme::parse_from_target(&self.target)
    }

    pub fn effective_pool_size(&self, scheme: TransportScheme) -> usize {
        let default_size = if scheme.is_grpc() {
            DEFAULT_GRPC_POOL_SIZE
        } else {
            DEFAULT_HTTP_POOL_SIZE
        };
        self.pool_size.unwrap_or(default_size).clamp(1, 128)
    }

    pub fn effective_max_yield_time_ms(&self) -> u32 {
        self.max_yield_time_ms
            .unwrap_or(DEFAULT_MAX_YIELD_TIME_MS)
            .clamp(1_000, HARD_MAX_YIELD_TIME_MS)
    }

    pub fn resolve_token(&self, fallback_token: Option<&str>) -> Result<Option<String>, ConfigError> {
        if let Some(tok) = &self.token {
            let trimmed = tok.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_owned()));
            }
        }
        if let Some(env_name) = &self.token_env {
            let env_key = env_name.trim();
            if !env_key.is_empty() {
                let val = std::env::var(env_key).map_err(|_| {
                    ConfigError::Invalid(format!(
                        "box '{}' requires token_env '{}' which is not set",
                        self.name, env_key
                    ))
                })?;
                let trimmed = val.trim();
                if trimmed.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "box '{}' requires token_env '{}' which is empty",
                        self.name, env_key
                    )));
                }
                return Ok(Some(trimmed.to_owned()));
            }
        }
        Ok(fallback_token
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    #[serde(default)]
    pub default_box: Option<String>,
    pub boxes: Vec<BoxConfig>,
}

#[derive(Debug, Clone)]
pub struct ValidatedClusterConfig {
    pub default_box: String,
    pub boxes: Vec<BoxConfig>,
}

pub fn validate_box_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty() {
        return Err(ConfigError::Invalid("box name cannot be empty".into()));
    }
    if name.len() > 64 {
        return Err(ConfigError::Invalid(format!(
            "box name '{name}' exceeds 64 characters"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(ConfigError::Invalid(format!(
            "box name '{name}' must contain only ASCII letters, digits, '-', '_', or '.' (no ':')"
        )));
    }
    Ok(())
}

impl ClusterConfig {
    pub fn from_json_str(raw: &str) -> Result<Self, ConfigError> {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            let boxes: Vec<BoxConfig> = serde_json::from_str(trimmed)?;
            Ok(Self {
                default_box: None,
                boxes,
            })
        } else {
            Ok(serde_json::from_str(trimmed)?)
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Io {
            path: path_ref.display().to_string(),
            source,
        })?;
        Self::from_json_str(&raw)
    }

    pub fn single_box(
        name: Option<&str>,
        target: String,
        token: Option<String>,
    ) -> Self {
        let box_name = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_BOX_NAME)
            .to_owned();
        Self {
            default_box: Some(box_name.clone()),
            boxes: vec![BoxConfig {
                name: box_name,
                target,
                token,
                token_env: None,
                description: None,
                labels: BTreeMap::new(),
                pool_size: None,
                max_yield_time_ms: None,
                ca_cert_path: None,
            }],
        }
    }

    pub fn validate(
        mut self,
        default_box_override: Option<&str>,
    ) -> Result<ValidatedClusterConfig, ConfigError> {
        if self.boxes.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one devbox must be configured".into(),
            ));
        }
        let mut seen = HashSet::with_capacity(self.boxes.len());
        for box_cfg in &mut self.boxes {
            box_cfg.name = box_cfg.name.trim().to_owned();
            box_cfg.target = box_cfg.target.trim().trim_end_matches('/').to_owned();
            validate_box_name(&box_cfg.name)?;
            if !seen.insert(box_cfg.name.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate box name '{}'",
                    box_cfg.name
                )));
            }
            let _ = box_cfg.scheme()?;
            if let Some((scheme, rest)) = box_cfg.target.split_once("://") {
                box_cfg.target = format!("{}://{rest}", scheme.to_ascii_lowercase());
            }
        }

        let resolved_default = default_box_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.default_box
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| self.boxes[0].name.clone());

        if !seen.contains(&resolved_default) {
            let names: Vec<&str> = self.boxes.iter().map(|b| b.name.as_str()).collect();
            return Err(ConfigError::Invalid(format!(
                "default_box '{}' is not in configured boxes [{}]",
                resolved_default,
                names.join(", ")
            )));
        }

        Ok(ValidatedClusterConfig {
            default_box: resolved_default,
            boxes: self.boxes,
        })
    }
}
