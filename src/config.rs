//! Configuration model, (de)serialization, and model->backend resolution.
//!
//! Config lives at `<config_dir>/llm-hub/settings.json`, where `<config_dir>`
//! is platform specific (e.g. `~/.config` on Linux, `~/Library/Application Support`
//! on macOS — see [`dirs::config_dir`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Root configuration document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// The list of upstream backends.
    #[serde(default)]
    pub backends: Vec<Backend>,
}

/// A single upstream LLM backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    /// Human-readable name, e.g. "硅流".
    pub name: String,
    /// Base URL of the OpenAI-compatible API, e.g. `https://api.siliconflow.cn`.
    pub base_url: String,
    /// One or more API keys; tried in order on failure.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Models served by this backend, e.g. `["qwen3.6-27b", "deepseek-v4pro"]`.
    #[serde(default)]
    pub models: Vec<String>,
}

/// A backend resolved for a specific model: exactly one concrete endpoint
/// (base url) paired with exactly one key. A [`Backend`] with N keys that
/// serves a model yields N [`ResolvedBackend`] entries.
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    /// Name of the source backend (copied from [`Backend::name`]).
    pub backend_name: String,
    /// Base URL with no trailing slash.
    pub base_url: String,
    /// The single API key to authenticate with.
    pub key: String,
}

impl Config {
    /// Load the config from the default path. Returns a default (empty) config
    /// when the file does not exist yet, rather than an error.
    pub fn load() -> Result<Self> {
        match Self::path() {
            Some(p) => {
                if !p.exists() {
                    return Ok(Config::default());
                }
                let raw = std::fs::read_to_string(&p)?;
                if raw.trim().is_empty() {
                    return Ok(Config::default());
                }
                let cfg: Config = serde_json::from_str(&raw)?;
                Ok(cfg)
            }
            None => Ok(Config::default()),
        }
    }

    /// Save the config to the default path, creating parent directories.
    pub fn save(&self) -> Result<()> {
        let path = Self::path().ok_or_else(|| Error::Config(
            "could not determine the configuration directory for this platform".into(),
        ))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pretty = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, pretty)?;
        Ok(())
    }

    /// The default config file path, if the platform config dir is known.
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("llm-hub").join("settings.json"))
    }

    /// Return a config seeded with a sample backend, useful as an initial file.
    pub fn sample() -> Self {
        Config {
            backends: vec![Backend {
                name: "硅流".into(),
                base_url: "https://api.siliconflow.cn".into(),
                keys: vec!["sk-your-api-key".into()],
                models: vec!["Qwen/Qwen3-32B".into(), "deepseek-ai/DeepSeek-V3".into()],
            }],
        }
    }
}

/// Round-robin load balancer + model resolver, shared across requests via `Arc`.
///
/// Each backend owns a monotonically increasing counter; for every request the
/// backend's key list is rotated by its counter so the starting key advances
/// (round-robin load balancing). All keys of each backend remain available for
/// failover within a single request, in rotated order.
pub struct Resolver {
    config: Arc<Config>,
    counters: Vec<AtomicUsize>,
}

impl Resolver {
    /// Build a resolver sharing the given config. One atomic counter per backend
    /// (indexed identically to `config.backends`).
    pub fn new(config: Arc<Config>) -> Self {
        let counters = (0..config.backends.len())
            .map(|_| AtomicUsize::new(0))
            .collect();
        Self { config, counters }
    }

    /// Borrow the underlying config (used e.g. by `/v1/models`).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolve the ordered failover list for `model`.
    ///
    /// Iterates backends in configuration order; for each backend that lists
    /// `model`, emits one [`ResolvedBackend`] per non-empty key, with the key list
    /// rotated by that backend's counter (so the first key changes each call).
    /// Backends with no non-empty key are skipped.
    pub fn resolve(&self, model: &str) -> Vec<ResolvedBackend> {
        let mut out = Vec::new();
        for (i, b) in self.config.backends.iter().enumerate() {
            if !b.models.iter().any(|m| m == model) {
                continue;
            }
            let keys: Vec<&String> = b.keys.iter().filter(|k| !k.trim().is_empty()).collect();
            if keys.is_empty() {
                continue;
            }
            let base_url = b.base_url.trim_end_matches('/').to_owned();
            let start = self.counters[i].fetch_add(1, Ordering::Relaxed);
            for j in 0..keys.len() {
                let key = keys[(start + j) % keys.len()];
                out.push(ResolvedBackend {
                    backend_name: b.name.clone(),
                    base_url: base_url.clone(),
                    key: key.clone(),
                });
            }
        }
        out
    }
}
