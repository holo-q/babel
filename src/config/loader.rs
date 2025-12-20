//! Configuration file loader with graceful fallback to defaults

use std::path::PathBuf;
use anyhow::{Context, Result};
use tracing::{info, warn};

use super::schema::BabelConfig;

/// Returns the path to the babel configuration file
///
/// Located at ~/.config/babel/babel.toml
pub fn config_path() -> PathBuf {
    let home = std::env::var("HOME")
        .expect("HOME environment variable must be set");
    PathBuf::from(home)
        .join(".config")
        .join("babel")
        .join("babel.toml")
}

/// Load babel configuration from ~/.config/babel/babel.toml
///
/// Returns default configuration if the file doesn't exist or cannot be read.
/// This allows babel to run with sensible defaults out of the box.
///
/// # Errors
///
/// Only returns errors for malformed TOML (parsing errors).
/// Missing files are handled gracefully by returning defaults.
pub fn load_config() -> Result<BabelConfig> {
    let path = config_path();

    if !path.exists() {
        info!(
            path = %path.display(),
            "Config file not found, using defaults"
        );
        return Ok(BabelConfig::default());
    }

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let config: BabelConfig = toml::from_str(&contents)
                .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

            info!(
                path = %path.display(),
                title_policy_enabled = config.title_policy.enabled,
                policy = %config.title_policy.policy,
                "Loaded babel configuration"
            );

            Ok(config)
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Failed to read config file, using defaults"
            );
            Ok(BabelConfig::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_path() {
        let path = config_path();
        assert!(path.ends_with(".config/babel/babel.toml"));
    }

    #[test]
    fn test_load_defaults_when_missing() {
        // This should not panic even if config doesn't exist
        let config = load_config().expect("Should return defaults");
        assert!(config.title_policy.enabled);
        assert_eq!(config.title_policy.policy, "rolling_prompts");
    }

    #[test]
    fn test_default_config_serializes() {
        // Ensure defaults can round-trip through TOML
        let config = BabelConfig::default();
        let toml_str = toml::to_string_pretty(&config)
            .expect("Should serialize to TOML");

        let parsed: BabelConfig = toml::from_str(&toml_str)
            .expect("Should parse back from TOML");

        assert_eq!(parsed.title_policy.enabled, config.title_policy.enabled);
        assert_eq!(parsed.title_policy.rolling_prompts.prompt_count, 4);
    }
}
