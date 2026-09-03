use serde::Deserialize;
use std::{collections::HashMap, env, fs, net::IpAddr, path::Path};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub api: ApiConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub virtual_models: HashMap<String, VirtualModelConfig>,
    #[serde(default)]
    pub aliases: Vec<ModelAlias>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_gateway_key_env")]
    pub key_env: String,
    #[serde(default = "default_model")]
    pub default_model: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_database_url")]
    pub database_url: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            database_url: default_database_url(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_context_target_tokens")]
    pub target_tokens: usize,
    #[serde(default = "default_context_reserve_output_tokens")]
    pub reserve_output_tokens: usize,
    #[serde(default = "default_context_recent_messages")]
    pub recent_messages: usize,
    #[serde(default = "default_context_trigger_ratio")]
    pub compaction_trigger_ratio: f64,
    #[serde(default = "default_context_summary_input_tokens")]
    pub summary_input_tokens: usize,
    #[serde(default = "default_context_summary_max_tokens")]
    pub summary_max_tokens: usize,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default = "default_true")]
    pub retrieval_enabled: bool,
    #[serde(default = "default_retrieval_top_k")]
    pub retrieval_top_k: usize,
    #[serde(default = "default_retrieval_max_tokens")]
    pub retrieval_max_tokens: usize,
    #[serde(default = "default_retrieval_min_score")]
    pub retrieval_min_score: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_tokens: default_context_target_tokens(),
            reserve_output_tokens: default_context_reserve_output_tokens(),
            recent_messages: default_context_recent_messages(),
            compaction_trigger_ratio: default_context_trigger_ratio(),
            summary_input_tokens: default_context_summary_input_tokens(),
            summary_max_tokens: default_context_summary_max_tokens(),
            summary_model: None,
            retrieval_enabled: true,
            retrieval_top_k: default_retrieval_top_k(),
            retrieval_max_tokens: default_retrieval_max_tokens(),
            retrieval_min_score: default_retrieval_min_score(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
    #[serde(default = "default_models_path")]
    pub models_path: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub provider: String,
    pub api_key_env: String,
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub discover_models: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RouteConfig {
    pub id: String,
    pub account: String,
    pub model: String,
    #[serde(default = "default_priority")]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct VirtualModelConfig {
    pub routes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ModelAlias {
    pub pattern: String,
    pub target: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("required environment variable {0} is not set")]
    MissingEnv(String),
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&raw)?;
        config.validate()?;
        Ok(config)
    }

    pub fn gateway_api_key(&self) -> Result<String, ConfigError> {
        env::var(&self.api.key_env).map_err(|_| ConfigError::MissingEnv(self.api.key_env.clone()))
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn account(&self, id: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|account| account.id == id)
    }

    pub fn route(&self, id: &str) -> Option<&RouteConfig> {
        self.routes.iter().find(|route| route.id == id)
    }

    pub fn resolve_model_alias<'a>(&'a self, requested: &'a str) -> &'a str {
        self.aliases
            .iter()
            .find(|alias| pattern_matches(&alias.pattern, requested))
            .map(|alias| alias.target.as_str())
            .unwrap_or(requested)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::Invalid("at least one provider is required".into()));
        }
        if self.accounts.is_empty() {
            return Err(ConfigError::Invalid("at least one account is required".into()));
        }
        if self.routes.is_empty() {
            return Err(ConfigError::Invalid("at least one route is required".into()));
        }
        if !self.virtual_models.contains_key(&self.api.default_model) {
            return Err(ConfigError::Invalid(format!(
                "default model '{}' is not defined in [virtual_models]",
                self.api.default_model
            )));
        }
        if !(0.5..=1.0).contains(&self.context.compaction_trigger_ratio) {
            return Err(ConfigError::Invalid(
                "context.compaction_trigger_ratio must be between 0.5 and 1.0".into(),
            ));
        }
        if self.context.target_tokens < 1024 {
            return Err(ConfigError::Invalid(
                "context.target_tokens must be at least 1024".into(),
            ));
        }
        if self.context.recent_messages == 0 {
            return Err(ConfigError::Invalid(
                "context.recent_messages must be greater than zero".into(),
            ));
        }
        if self.context.summary_input_tokens < 512 {
            return Err(ConfigError::Invalid(
                "context.summary_input_tokens must be at least 512".into(),
            ));
        }
        if self.context.summary_max_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.summary_max_tokens must be greater than zero".into(),
            ));
        }
        if self.context.retrieval_top_k > 20 {
            return Err(ConfigError::Invalid(
                "context.retrieval_top_k must be <= 20".into(),
            ));
        }
        if self.context.retrieval_max_tokens > self.context.target_tokens {
            return Err(ConfigError::Invalid(
                "context.retrieval_max_tokens must not exceed context.target_tokens".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.context.retrieval_min_score) {
            return Err(ConfigError::Invalid(
                "context.retrieval_min_score must be between 0 and 1".into(),
            ));
        }

        for account in &self.accounts {
            if self.provider(&account.provider).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "account '{}' references unknown provider '{}'",
                    account.id, account.provider
                )));
            }
        }

        for route in &self.routes {
            if self.account(&route.account).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "route '{}' references unknown account '{}'",
                    route.id, route.account
                )));
            }
        }

        for (name, virtual_model) in &self.virtual_models {
            for route_id in &virtual_model.routes {
                if self.route(route_id).is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "virtual model '{}' references unknown route '{}'",
                        name, route_id
                    )));
                }
            }
        }

        Ok(())
    }
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        value.starts_with(prefix)
    } else {
        pattern == value
    }
}

fn default_host() -> IpAddr { "127.0.0.1".parse().expect("valid loopback address") }
fn default_port() -> u16 { 7331 }
fn default_gateway_key_env() -> String { "LLMGATEWAY_API_KEY".into() }
fn default_model() -> String { "llmgateway-auto".into() }
fn default_database_url() -> String { "sqlite://data/llmgateway.db".into() }
fn default_provider_kind() -> String { "openai-compatible".into() }
fn default_models_path() -> String { "models".into() }
fn default_auth_style() -> String { "bearer".into() }
fn default_priority() -> i32 { 100 }
fn default_true() -> bool { true }
fn default_context_target_tokens() -> usize { 16_000 }
fn default_context_reserve_output_tokens() -> usize { 4_000 }
fn default_context_recent_messages() -> usize { 12 }
fn default_context_trigger_ratio() -> f64 { 0.85 }
fn default_context_summary_input_tokens() -> usize { 12_000 }
fn default_context_summary_max_tokens() -> usize { 1_200 }
fn default_retrieval_top_k() -> usize { 4 }
fn default_retrieval_max_tokens() -> usize { 2_000 }
fn default_retrieval_min_score() -> f64 { 0.08 }

#[cfg(test)]
mod tests {
    use super::pattern_matches;

    #[test]
    fn wildcard_alias_matches_prefix() {
        assert!(pattern_matches("claude-sonnet-*", "claude-sonnet-4-5"));
        assert!(!pattern_matches("claude-sonnet-*", "claude-opus-4-1"));
    }
}
