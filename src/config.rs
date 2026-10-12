//! Configuration loading for Trandex.
//!
//! The schema mirrors `src/config.yaml`. `styles`, `position`, and `items` are
//! parsed and validated before the UI is constructed.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Placeholder used when the configuration does not set one.
const DEFAULT_PLACEHOLDER: &str = "Search files…";

/// Directories ignored while indexing when the configuration does not say otherwise.
const DEFAULT_IGNORE: &[&str] = &["node_modules", ".git"];

/// The Trandex configuration file.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Path to an external stylesheet, absolute or relative to this file.
    #[serde(default)]
    pub styles: Option<PathBuf>,
    /// Panel position: `center`, `top`, `left`, or `right`.
    #[serde(default)]
    pub position: Option<String>,
    /// Hint text shown in the search bar.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// Preview column: `left`, `right`, or `false`. Anything else is rejected.
    #[serde(default)]
    pub preview: Option<serde_yaml_ng::Value>,
    /// Item providers in display and ranking order.
    #[serde(default)]
    pub items: Vec<String>,
    /// Directory names excluded from indexing.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Whether hidden files are indexed. Defaults to `true`.
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    AppList,
    Math,
    Index,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPosition {
    Center,
    Top,
    Left,
    Right,
}

/// Where the placeholder preview column is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPosition {
    /// An empty, fixed-width box on the left of each row.
    Left,
    /// An empty, fixed-width box on the right of each row.
    Right,
    /// No preview column.
    None,
}

impl Config {
    /// Read and parse the configuration at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ConfigError::NotFound(path.to_path_buf())
            } else {
                ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;

        // An empty file is a valid "all defaults" configuration.
        if text.trim().is_empty() {
            return Ok(Self::default());
        }

        let config: Config =
            serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;

        config.validate().map_err(|error| match error {
            ValidationError::Preview(value) => ConfigError::InvalidPreview {
                path: path.to_path_buf(),
                value,
            },
            ValidationError::Position(value) => ConfigError::InvalidPosition {
                path: path.to_path_buf(),
                value,
            },
            ValidationError::Items(value) => ConfigError::InvalidItems {
                path: path.to_path_buf(),
                value,
            },
        })?;

        Ok(config)
    }

    /// Reject `preview` values other than `left`, `right`, or `false`.
    fn validate(&self) -> Result<(), ValidationError> {
        for item in &self.items {
            if !matches!(item.as_str(), "AppList" | "Math" | "Index") {
                return Err(ValidationError::Items(item.clone()));
            }
        }
        if let Some(position) = self.position.as_deref()
            && !matches!(position, "center" | "top" | "left" | "right")
        {
            return Err(ValidationError::Position(position.to_string()));
        }

        let Some(value) = &self.preview else {
            return Ok(());
        };

        let valid = match value {
            serde_yaml_ng::Value::String(word) => {
                word == "left" || word == "right" || word == "false"
            }
            serde_yaml_ng::Value::Bool(false) | serde_yaml_ng::Value::Null => true,
            _ => false,
        };

        if valid {
            Ok(())
        } else {
            Err(ValidationError::Preview(describe_value(value)))
        }
    }

    pub fn providers(&self) -> Vec<Provider> {
        if self.items.is_empty() {
            vec![Provider::AppList, Provider::Math, Provider::Index]
        } else {
            self.items.iter().filter_map(|item| match item.as_str() {
                "AppList" => Some(Provider::AppList),
                "Math" => Some(Provider::Math),
                "Index" => Some(Provider::Index),
                _ => None,
            }).collect()
        }
    }

    pub fn provider_enabled(&self, provider: Provider) -> bool {
        self.providers().contains(&provider)
    }

    pub fn window_position(&self) -> WindowPosition {
        match self.position.as_deref() {
            Some("top") => WindowPosition::Top,
            Some("left") => WindowPosition::Left,
            Some("right") => WindowPosition::Right,
            _ => WindowPosition::Center,
        }
    }

    /// Hint text for the search entry.
    pub fn placeholder(&self) -> &str {
        self.placeholder.as_deref().unwrap_or(DEFAULT_PLACEHOLDER)
    }

    /// Which (if any) side of each result row carries the preview box.
    pub fn preview_position(&self) -> PreviewPosition {
        match &self.preview {
            Some(serde_yaml_ng::Value::String(word)) if word == "left" => PreviewPosition::Left,
            Some(serde_yaml_ng::Value::String(word)) if word == "right" => PreviewPosition::Right,
            _ => PreviewPosition::None,
        }
    }

    /// Directory names to exclude while indexing.
    pub fn ignore_dirs(&self) -> Vec<String> {
        if self.ignore.is_empty() {
            DEFAULT_IGNORE.iter().map(|dir| dir.to_string()).collect()
        } else {
            self.ignore.clone()
        }
    }

    /// Whether hidden files should be indexed.
    pub fn hidden(&self) -> bool {
        self.hidden.unwrap_or(true)
    }
}

/// Errors that can occur while loading a [`Config`].
#[derive(Debug)]
pub enum ConfigError {
    /// No file exists at the given path.
    NotFound(PathBuf),
    /// The file exists but could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was read but does not match the schema.
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    /// The `preview` value is not one of `left`, `right`, or `false`.
    InvalidPreview { path: PathBuf, value: String },
    InvalidPosition { path: PathBuf, value: String },
    InvalidItems { path: PathBuf, value: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotFound(path) => write!(
                f,
                "config file not found: {}\n       create it, or pass --config <PATH>",
                path.display()
            ),
            ConfigError::Read { path, source } => {
                write!(f, "could not read config file {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                write!(f, "invalid config file {}: {source}", path.display())
            }
            ConfigError::InvalidPreview { path, value } => write!(
                f,
                "invalid config file {}: preview must be one of \"left\", \"right\", or \"false\", got \"{value}\"",
                path.display()
            ),
            ConfigError::InvalidPosition { path, value } => write!(
                f,
                "invalid config file {}: position must be one of \"center\", \"top\", \"left\", or \"right\", got \"{value}\"",
                path.display()
            ),
            ConfigError::InvalidItems { path, value } => write!(
                f,
                "invalid config file {}: items entry {value:?}; valid names are AppList, Math, Index",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Render a YAML value as a short, human-readable string for error messages.
fn describe_value(value: &serde_yaml_ng::Value) -> String {
    match value {
        serde_yaml_ng::Value::String(word) => word.clone(),
        serde_yaml_ng::Value::Bool(flag) => flag.to_string(),
        serde_yaml_ng::Value::Number(number) => number.to_string(),
        serde_yaml_ng::Value::Null => "null".to_string(),
        serde_yaml_ng::Value::Sequence(entries) => {
            format!("[{}]", entries.len())
        }

        serde_yaml_ng::Value::Mapping(entries) => {
            format!("{{{}}}", entries.len())
        }
        _ => "<complex value>".to_string(),
    }
}

enum ValidationError {
    Preview(String),
    Position(String),
    Items(String),
}
