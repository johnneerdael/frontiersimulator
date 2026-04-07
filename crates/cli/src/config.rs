use frontier_transport::{ProtocolMode, Sink};
use serde::Deserialize;
use std::path::Path;

/// Top-level configuration, loaded from TOML with env var and CLI overrides.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersConfig {
    pub realdebrid: Option<ProviderEntry>,
    pub premiumize: Option<ProviderEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_chunk_size_mb")]
    pub chunk_size_mb: u32,
    #[serde(default = "default_prefetch_chunk_size_mb")]
    pub prefetch_chunk_size_mb: u32,
    #[serde(default = "default_page_size_kb")]
    pub page_size_kb: u32,
    #[serde(default = "default_urgent_workers")]
    pub urgent_workers: u32,
    #[serde(default)]
    pub protocol: ProtocolMode,
    #[serde(default)]
    pub sink: Sink,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_trace_dir")]
    pub trace_dir: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

pub const FIXED_PREFETCH_WORKERS: u32 = 1;

fn default_chunk_size_mb() -> u32 {
    4
}
fn default_prefetch_chunk_size_mb() -> u32 {
    8
}
fn default_page_size_kb() -> u32 {
    128
}
fn default_urgent_workers() -> u32 {
    2
}
fn default_trace_dir() -> String {
    "./traces".to_string()
}
fn default_db_path() -> String {
    "./frontier.db".to_string()
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            chunk_size_mb: default_chunk_size_mb(),
            prefetch_chunk_size_mb: default_prefetch_chunk_size_mb(),
            page_size_kb: default_page_size_kb(),
            urgent_workers: default_urgent_workers(),
            protocol: ProtocolMode::default(),
            sink: Sink::default(),
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            trace_dir: default_trace_dir(),
            db_path: default_db_path(),
        }
    }
}

impl Config {
    /// Load config from TOML file with environment variable overrides.
    ///
    /// Layered config: TOML file < env vars < CLI flags (CLI handled by caller).
    /// - REALDEBRID_API_KEY overrides providers.realdebrid.api_key
    /// - PREMIUMIZE_API_KEY overrides providers.premiumize.api_key
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let mut config = if Path::new(path).exists() {
            let contents =
                std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
            toml::from_str::<Config>(&contents).map_err(|e| ConfigError::Parse(e.to_string()))?
        } else {
            // No config file — use defaults
            Config {
                providers: ProvidersConfig::default(),
                defaults: DefaultsConfig::default(),
                output: OutputConfig::default(),
            }
        };

        // Env var overrides for API keys
        if let Ok(key) = std::env::var("REALDEBRID_API_KEY") {
            config.providers.realdebrid = Some(ProviderEntry { api_key: key });
        }
        if let Ok(key) = std::env::var("PREMIUMIZE_API_KEY") {
            config.providers.premiumize = Some(ProviderEntry { api_key: key });
        }

        Ok(config)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(String),
    #[error("parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_without_config_file() {
        let config = Config::load("/nonexistent/path.toml").unwrap();
        assert_eq!(config.defaults.chunk_size_mb, 4);
        assert_eq!(config.defaults.prefetch_chunk_size_mb, 8);
        assert_eq!(config.defaults.page_size_kb, 128);
        assert_eq!(config.defaults.urgent_workers, 2);
        assert_eq!(config.defaults.protocol, ProtocolMode::Auto);
        assert_eq!(config.defaults.sink, Sink::Discard);
    }

    #[test]
    fn env_overrides_api_key() {
        std::env::set_var("REALDEBRID_API_KEY", "test_key_123");
        let config = Config::load("/nonexistent/path.toml").unwrap();
        let rd = config.providers.realdebrid.unwrap();
        assert_eq!(rd.api_key, "test_key_123");
        std::env::remove_var("REALDEBRID_API_KEY");
    }

    #[test]
    fn parses_toml() {
        let toml_str = r#"
[providers.realdebrid]
api_key = "rd_key"

[providers.premiumize]
api_key = "pm_key"

[defaults]
chunk_size_mb = 8
prefetch_chunk_size_mb = 16
page_size_kb = 128
urgent_workers = 3
prefetch_workers = 2
protocol = "h2"
sink = "discard"

[output]
trace_dir = "/tmp/traces"
db_path = "/tmp/frontier.db"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.defaults.chunk_size_mb, 8);
        assert_eq!(config.defaults.prefetch_chunk_size_mb, 16);
        assert_eq!(config.defaults.urgent_workers, 3);
        assert_eq!(config.defaults.protocol, ProtocolMode::H2);
        assert_eq!(config.output.trace_dir, "/tmp/traces");
        assert_eq!(
            config.providers.realdebrid.as_ref().unwrap().api_key,
            "rd_key"
        );
    }

    #[test]
    fn legacy_prefetch_workers_input_does_not_change_fixed_runtime_value() {
        let toml_str = r#"
[defaults]
chunk_size_mb = 4
prefetch_chunk_size_mb = 8
urgent_workers = 2
prefetch_workers = 4
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.defaults.prefetch_chunk_size_mb, 8);
        assert_eq!(FIXED_PREFETCH_WORKERS, 1);
    }
}
