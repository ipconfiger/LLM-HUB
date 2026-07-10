//! Configuration model, (de)serialization, and model->backend resolution.
//!
//! Config lives at `<config_dir>/llm-hub/settings.json`, where `<config_dir>`
//! is platform specific (e.g. `~/.config` on Linux, `~/Library/Application Support`
//! on macOS — see [`dirs::config_dir`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// One concrete API key resolved for a backend, carrying the index of the key
/// within its backend's non-empty key list (so the proxy can report outcomes
/// back to the [`Resolver`]).
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    /// Index into this backend's filtered (non-empty) key list.
    pub key_index: usize,
    /// The API key value.
    pub key: String,
}

/// A backend resolved for a specific model: an endpoint (base url) plus the
/// ordered list of keys to try. The keys are ordered sticky-pointer-first.
#[derive(Debug, Clone)]
pub struct ResolvedBackend {
    /// Index into `config.backends`.
    pub backend_index: usize,
    /// Name of the source backend (copied from [`Backend::name`]).
    pub backend_name: String,
    /// Base URL with no trailing slash.
    pub base_url: String,
    /// Keys to try, ordered with the currently-preferred (sticky) key first.
    pub keys: Vec<ResolvedKey>,
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
        let path = Self::path().ok_or_else(|| {
            Error::Config(
                "could not determine the configuration directory for this platform".into(),
            )
        })?;
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

/// Per-key health bookkeeping within a [`BackendState`].
struct KeyState {
    /// The (non-empty) key value, remembered at construction.
    key: String,
    /// When this key's exhaustion cooldown expires; `None` means healthy.
    exhausted_until: Option<Instant>,
}

/// Per-backend health bookkeeping within [`ResolverState`].
struct BackendState {
    /// Non-empty keys of this backend (empties filtered once at construction).
    /// `key_index` everywhere refers to offsets within this list.
    keys: Vec<KeyState>,
    /// Preferred (sticky) key index into `keys`; tried first while healthy.
    sticky: usize,
    /// When the whole backend's unreachable cooldown expires; `None` means up.
    unreachable_until: Option<Instant>,
}

/// Mutable resolver state, guarded by the [`Resolver`]'s mutex.
struct ResolverState {
    /// One entry per `config.backends`, indexed identically.
    backends: Vec<BackendState>,
}

/// Sticky-key resolver + health tracker, shared across requests via `Arc`.
///
/// The same key is reused across requests until it signals exhaustion
/// (401/402/403/429); then the next key of the same backend is tried. A backend
/// that is unreachable (transport error or HTTP >= 500) is parked wholesale and
/// its remaining keys are skipped. Exhausted keys and unreachable backends are
/// cooled down (60s / 10s by default) and re-enter the pool automatically once
/// their cooldown elapses.
pub struct Resolver {
    config: Arc<Config>,
    state: Mutex<ResolverState>,
    key_cooldown: Duration,
    backend_cooldown: Duration,
}

impl Resolver {
    /// Build a resolver with the default cooldowns (key: 60s, backend: 10s).
    pub fn new(config: Arc<Config>) -> Self {
        Self::new_with_cooldowns(config, Duration::from_secs(60), Duration::from_secs(10))
    }

    /// Build a resolver with explicit cooldowns (primarily for testing).
    pub fn new_with_cooldowns(
        config: Arc<Config>,
        key_cooldown: Duration,
        backend_cooldown: Duration,
    ) -> Self {
        let backends = config
            .backends
            .iter()
            .map(|b| {
                // Filter empties once; key_index everywhere refers to this list.
                let keys = b
                    .keys
                    .iter()
                    .filter(|k| !k.trim().is_empty())
                    .map(|k| KeyState {
                        key: k.clone(),
                        exhausted_until: None,
                    })
                    .collect();
                BackendState {
                    keys,
                    sticky: 0,
                    unreachable_until: None,
                }
            })
            .collect();
        Self {
            config,
            state: Mutex::new(ResolverState { backends }),
            key_cooldown,
            backend_cooldown,
        }
    }

    /// Borrow the underlying config (used e.g. by `/v1/models`).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Resolve candidate backends for `model`. Returns backends that serve the
    /// model (config order). Each backend's keys are ordered with the sticky
    /// pointer first (rotated), and keys still within their exhaustion cooldown
    /// are omitted. Backends currently parked (unreachable, within cooldown) are
    /// omitted — UNLESS every serving backend is parked, in which case all are
    /// returned (last-resort fallback so we still attempt rather than hard-fail).
    pub fn resolve(&self, model: &str) -> Vec<ResolvedBackend> {
        let now = Instant::now();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Serving backend indices (config order), regardless of health.
        let serving: Vec<usize> = self
            .config
            .backends
            .iter()
            .enumerate()
            .filter(|(_, b)| b.models.iter().any(|m| m == model))
            .map(|(i, _)| i)
            .collect();
        if serving.is_empty() {
            return Vec::new();
        }

        let backend_parked = |i: usize| {
            state.backends[i]
                .unreachable_until
                .map(|t| t > now)
                .unwrap_or(false)
        };
        // Last-resort fallback: if every serving backend is parked, try them all.
        let all_parked = serving.iter().all(|&i| backend_parked(i));

        let mut out = Vec::new();
        for &i in &serving {
            if backend_parked(i) && !all_parked {
                continue;
            }
            let bs = &state.backends[i];
            let n = bs.keys.len();
            if n == 0 {
                continue;
            }
            // Rotation order: sticky first, then the rest, wrapping.
            let order = (0..n).map(|j| (bs.sticky + j) % n);
            // Drop keys still cooling down — unless this is the all-parked
            // fallback, where we try everything we have.
            let picked: Vec<usize> = if all_parked {
                order.collect()
            } else {
                order
                    .filter(|&k| bs.keys[k].exhausted_until.map(|t| t <= now).unwrap_or(true))
                    .collect()
            };
            if picked.is_empty() {
                continue;
            }
            let keys = picked
                .into_iter()
                .map(|k| ResolvedKey {
                    key_index: k,
                    key: bs.keys[k].key.clone(),
                })
                .collect();
            out.push(ResolvedBackend {
                backend_index: i,
                backend_name: self.config.backends[i].name.clone(),
                base_url: self.config.backends[i]
                    .base_url
                    .trim_end_matches('/')
                    .to_owned(),
                keys,
            });
        }
        out
    }

    /// A successful (2xx) attempt: clear any exhaustion/unreachable marks for
    /// this backend+key (the key is healthy again).
    pub fn mark_key_ok(&self, backend_index: usize, key_index: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bs) = state.backends.get_mut(backend_index) {
            if let Some(ks) = bs.keys.get_mut(key_index) {
                ks.exhausted_until = None;
            }
            bs.unreachable_until = None;
        }
    }

    /// A key-exhausted attempt (401/402/403/429): park this key until
    /// `now + key_cooldown`, and advance the sticky pointer to the next key.
    pub fn mark_key_exhausted(&self, backend_index: usize, key_index: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bs) = state.backends.get_mut(backend_index) {
            if let Some(ks) = bs.keys.get_mut(key_index) {
                ks.exhausted_until = Some(Instant::now() + self.key_cooldown);
            }
            if !bs.keys.is_empty() {
                bs.sticky = (key_index + 1) % bs.keys.len();
            }
        }
    }

    /// A backend-unreachable attempt (transport error or HTTP >= 500): park the
    /// whole backend until `now + backend_cooldown` (its remaining keys are
    /// skipped for the rest of the current request).
    pub fn mark_backend_unreachable(&self, backend_index: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bs) = state.backends.get_mut(backend_index) {
            bs.unreachable_until = Some(Instant::now() + self.backend_cooldown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(name: &str, url: &str, keys: &[&str], models: &[&str]) -> Backend {
        Backend {
            name: name.into(),
            base_url: url.into(),
            keys: keys.iter().map(|s| (*s).into()).collect(),
            models: models.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn key_indices(b: &ResolvedBackend) -> Vec<usize> {
        b.keys.iter().map(|k| k.key_index).collect()
    }

    #[test]
    fn resolve_initial_order_is_identity() {
        let cfg = Arc::new(Config {
            backends: vec![backend("b0", "https://x", &["k0", "k1", "k2"], &["m"])],
        });
        let r = Resolver::new(cfg);
        let groups = r.resolve("m");
        assert_eq!(groups.len(), 1);
        // sticky defaults to 0 → keys in natural order.
        assert_eq!(key_indices(&groups[0]), vec![0, 1, 2]);
    }

    #[test]
    fn resolve_rotates_sticky_first_and_parks_exhausted_key() {
        let cfg = Arc::new(Config {
            backends: vec![backend("b0", "https://x", &["k0", "k1", "k2"], &["m"])],
        });
        let r = Resolver::new(cfg);

        // Exhaust key 0: it is parked (60s) and sticky advances to 1.
        r.mark_key_exhausted(0, 0);
        let groups = r.resolve("m");
        assert_eq!(groups.len(), 1);
        // key 0 is omitted (within cooldown); sticky(1) comes first.
        assert_eq!(key_indices(&groups[0]), vec![1, 2]);
    }

    #[test]
    fn mark_key_ok_clears_exhaustion() {
        let cfg = Arc::new(Config {
            backends: vec![backend("b0", "https://x", &["k0", "k1", "k2"], &["m"])],
        });
        let r = Resolver::new(cfg);
        r.mark_key_exhausted(0, 0);
        // key 0 reappears once cleared...
        r.mark_key_ok(0, 0);
        // ...but sticky is still 1 (mark_key_ok doesn't move the pointer), so the
        // order is [1, 2, 0] with all three keys present again.
        let groups = r.resolve("m");
        assert_eq!(key_indices(&groups[0]), vec![1, 2, 0]);
    }

    #[test]
    fn resolve_omits_unreachable_backend() {
        let cfg = Arc::new(Config {
            backends: vec![
                backend("a", "https://a", &["ak"], &["m"]),
                backend("b", "https://b", &["bk"], &["m"]),
            ],
        });
        let r = Resolver::new(cfg);
        assert_eq!(r.resolve("m").len(), 2);

        // Park backend "a" (index 0): it is omitted while "b" is still up.
        r.mark_backend_unreachable(0);
        let groups = r.resolve("m");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].backend_name, "b");
    }

    #[test]
    fn resolve_fallback_returns_all_when_every_backend_parked() {
        let cfg = Arc::new(Config {
            backends: vec![backend("solo", "https://x", &["k0"], &["m"])],
        });
        let r = Resolver::new(cfg);
        r.mark_backend_unreachable(0);
        // Only serving backend is parked → last-resort fallback still returns it.
        let groups = r.resolve("m");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].backend_name, "solo");
    }

    #[test]
    fn resolve_skips_non_serving_and_empty_key_backends() {
        let cfg = Arc::new(Config {
            backends: vec![
                backend("noserve", "https://x", &["k"], &["other"]),
                backend("emptykeys", "https://y", &["", "  "], &["m"]),
                backend("good", "https://z", &["g0"], &["m"]),
            ],
        });
        let r = Resolver::new(cfg);
        let groups = r.resolve("m");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].backend_name, "good");
    }
}
