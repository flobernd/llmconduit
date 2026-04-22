use serde::Deserialize;
use serde::Serialize;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub upstream_base_url: Url,
    pub upstream_api_key: Option<String>,
    pub upstream_model: Option<String>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    pub model_profiles: BTreeMap<String, ModelProfile>,
    pub brave_base_url: Url,
    pub brave_api_key: Option<String>,
    pub brave_max_results: usize,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PersistedModelProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
}

#[derive(Debug, Clone)]
pub struct ModelProfile {
    pub upstream_model: Option<String>,
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedConfig {
    pub bind_addr: String,
    pub upstream_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model: Option<String>,
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub upstream_chat_kwargs: JsonMap<String, JsonValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_profiles: BTreeMap<String, PersistedModelProfile>,
    pub brave_base_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brave_api_key: Option<String>,
    pub brave_max_results: usize,
    pub request_timeout_secs: u64,
}

impl Default for PersistedConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:4000".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            upstream_chat_kwargs: JsonMap::new(),
            model_profiles: BTreeMap::new(),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
        }
    }
}

impl Config {
    pub fn from_env_and_file(path: Option<&Path>) -> Result<Self, String> {
        let mut persisted = if let Some(path) = path {
            load_persisted_config(path)?
        } else {
            load_default_persisted_config()?
        };
        apply_env_overrides(&mut persisted);
        Self::from_persisted(&persisted)
    }

    pub fn from_persisted(config: &PersistedConfig) -> Result<Self, String> {
        let bind_addr = config
            .bind_addr
            .parse()
            .map_err(|err| format!("invalid bind_addr: {err}"))?;
        let upstream_base_url = Url::parse(&config.upstream_base_url)
            .map_err(|err| format!("invalid upstream_base_url: {err}"))?;
        let brave_base_url = Url::parse(&config.brave_base_url)
            .map_err(|err| format!("invalid brave_base_url: {err}"))?;
        Ok(Self {
            bind_addr,
            upstream_base_url,
            upstream_api_key: config
                .upstream_api_key
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            upstream_model: config
                .upstream_model
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            upstream_chat_kwargs: config.upstream_chat_kwargs.clone(),
            model_profiles: config
                .model_profiles
                .iter()
                .filter_map(|(name, profile)| {
                    let name = name.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some((
                        name.to_string(),
                        ModelProfile {
                            upstream_model: profile
                                .upstream_model
                                .as_ref()
                                .map(|value| value.trim().to_string())
                                .filter(|value| !value.is_empty()),
                            upstream_chat_kwargs: profile.upstream_chat_kwargs.clone(),
                        },
                    ))
                })
                .collect(),
            brave_base_url,
            brave_api_key: config
                .brave_api_key
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            brave_max_results: config.brave_max_results,
            request_timeout: Duration::from_secs(config.request_timeout_secs),
        })
    }

    pub fn resolve_upstream_model(&self, request_model: &str) -> String {
        self.model_profiles
            .get(request_model)
            .and_then(|profile| profile.upstream_model.clone())
            .or_else(|| self.upstream_model.clone())
            .unwrap_or_else(|| request_model.to_string())
    }

    pub fn resolve_upstream_chat_kwargs(&self, request_model: &str) -> JsonMap<String, JsonValue> {
        let mut kwargs = self.upstream_chat_kwargs.clone();
        if let Some(profile) = self.model_profiles.get(request_model) {
            merge_json_maps(&mut kwargs, &profile.upstream_chat_kwargs);
        }
        kwargs
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "unable to determine configuration directory".to_string())?;
    Ok(config_dir.join("resp2chat").join("config.yaml"))
}

pub fn load_default_persisted_config() -> Result<PersistedConfig, String> {
    let path = default_config_path()?;
    load_persisted_config(&path)
}

pub fn load_persisted_config(path: &Path) -> Result<PersistedConfig, String> {
    if !path.exists() {
        return Ok(PersistedConfig::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_yaml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))
}

pub fn write_persisted_config(path: &Path, config: &PersistedConfig) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    let yaml = serde_yaml::to_string(config)
        .map_err(|err| format!("failed to serialize config: {err}"))?;
    fs::write(path, yaml).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn apply_env_overrides(config: &mut PersistedConfig) {
    if let Ok(value) = env::var("RESP2CHAT_BIND_ADDR")
        && !value.trim().is_empty()
    {
        config.bind_addr = value;
    }
    if let Ok(value) = env::var("RESP2CHAT_UPSTREAM_BASE_URL")
        && !value.trim().is_empty()
    {
        config.upstream_base_url = value;
    }
    if let Ok(value) = env::var("RESP2CHAT_UPSTREAM_API_KEY")
        && !value.trim().is_empty()
    {
        config.upstream_api_key = Some(value);
    } else if config.upstream_api_key.is_none()
        && let Ok(value) = env::var("OPENAI_API_KEY")
        && !value.trim().is_empty()
    {
        config.upstream_api_key = Some(value);
    }
    if let Ok(value) = env::var("RESP2CHAT_UPSTREAM_MODEL")
        && !value.trim().is_empty()
    {
        config.upstream_model = Some(value);
    }
    if let Ok(value) = env::var("RESP2CHAT_UPSTREAM_CHAT_KWARGS_JSON")
        && !value.trim().is_empty()
        && let Ok(parsed) = serde_json::from_str::<JsonMap<String, JsonValue>>(&value)
    {
        config.upstream_chat_kwargs = parsed;
    }
    if let Ok(value) = env::var("RESP2CHAT_BRAVE_BASE_URL")
        && !value.trim().is_empty()
    {
        config.brave_base_url = value;
    }
    if let Ok(value) = env::var("BRAVE_SEARCH_API_KEY")
        && !value.trim().is_empty()
    {
        config.brave_api_key = Some(value);
    }
    if let Ok(value) = env::var("RESP2CHAT_BRAVE_MAX_RESULTS")
        && let Ok(parsed) = value.parse()
    {
        config.brave_max_results = parsed;
    }
    if let Ok(value) = env::var("RESP2CHAT_REQUEST_TIMEOUT_SECS")
        && let Ok(parsed) = value.parse()
    {
        config.request_timeout_secs = parsed;
    }
}

pub fn merge_json_maps(
    destination: &mut JsonMap<String, JsonValue>,
    source: &JsonMap<String, JsonValue>,
) {
    for (key, source_value) in source {
        match (destination.get_mut(key), source_value) {
            (Some(JsonValue::Object(destination_object)), JsonValue::Object(source_object)) => {
                merge_json_maps(destination_object, source_object);
            }
            _ => {
                destination.insert(key.clone(), source_value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use super::JsonMap;
    use super::JsonValue;
    use super::PersistedConfig;
    use super::PersistedModelProfile;
    use super::load_persisted_config;
    use super::merge_json_maps;
    use super::write_persisted_config;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn persisted_config_roundtrips() {
        let path = std::env::temp_dir().join(format!(
            "resp2chat-config-{}.yaml",
            uuid::Uuid::new_v4().simple()
        ));
        let config = PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: Some("upstream-secret".to_string()),
            upstream_model: Some("grok-4".to_string()),
            upstream_chat_kwargs: JsonMap::from_iter([(
                "clear_thinking".to_string(),
                JsonValue::Bool(false),
            )]),
            model_profiles: BTreeMap::from_iter([(
                "Kimi-K2.6".to_string(),
                PersistedModelProfile {
                    upstream_model: None,
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({
                            "thinking": true,
                            "preserve_thinking": true
                        }),
                    )]),
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: Some("secret".to_string()),
            brave_max_results: 7,
            request_timeout_secs: 45,
        };
        write_persisted_config(&path, &config).expect("write config");
        let loaded = load_persisted_config(&path).expect("load config");
        assert_eq!(loaded, config);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resolves_profile_specific_upstream_chat_kwargs() {
        let config = Config::from_persisted(&PersistedConfig {
            bind_addr: "127.0.0.1:4010".to_string(),
            upstream_base_url: "http://127.0.0.1:8000/v1".to_string(),
            upstream_api_key: None,
            upstream_model: None,
            upstream_chat_kwargs: JsonMap::new(),
            model_profiles: BTreeMap::from_iter([(
                "Kimi-K2.6".to_string(),
                PersistedModelProfile {
                    upstream_model: None,
                    upstream_chat_kwargs: JsonMap::from_iter([(
                        "chat_template_kwargs".to_string(),
                        json!({
                            "thinking": true,
                            "preserve_thinking": true
                        }),
                    )]),
                },
            )]),
            brave_base_url: "https://api.search.brave.com/res/v1".to_string(),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout_secs: 60,
        })
        .expect("config");

        assert_eq!(
            config.resolve_upstream_model("Kimi-K2.6"),
            "Kimi-K2.6".to_string()
        );
        assert_eq!(
            config.resolve_upstream_chat_kwargs("Kimi-K2.6"),
            JsonMap::from_iter([(
                "chat_template_kwargs".to_string(),
                json!({
                    "thinking": true,
                    "preserve_thinking": true
                }),
            )])
        );
    }

    #[test]
    fn merges_nested_profile_chat_kwargs() {
        let mut destination = JsonMap::from_iter([
            (
                "chat_template_kwargs".to_string(),
                json!({
                    "enable_thinking": true,
                    "clear_thinking": false
                }),
            ),
            ("stream_reasoning".to_string(), JsonValue::Bool(true)),
        ]);
        let source = JsonMap::from_iter([(
            "chat_template_kwargs".to_string(),
            json!({
                "thinking": true,
                "preserve_thinking": true
            }),
        )]);

        merge_json_maps(&mut destination, &source);

        assert_eq!(
            destination,
            JsonMap::from_iter([
                (
                    "chat_template_kwargs".to_string(),
                    json!({
                        "enable_thinking": true,
                        "clear_thinking": false,
                        "thinking": true,
                        "preserve_thinking": true
                    }),
                ),
                ("stream_reasoning".to_string(), JsonValue::Bool(true)),
            ])
        );
    }
}
