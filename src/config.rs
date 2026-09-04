use serde::Deserialize;
use std::{collections::{HashMap, HashSet}, env, fs, net::IpAddr, path::Path};
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
    pub routing: RoutingConfig,
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
    #[serde(default = "default_context_retrieval_max_chunks")]
    pub retrieval_max_chunks: usize,
    #[serde(default = "default_context_retrieval_max_tokens")]
    pub retrieval_max_tokens: usize,
    #[serde(default = "default_context_retrieval_min_score")]
    pub retrieval_min_score: f64,
    #[serde(default = "default_context_retrieval_backend")]
    pub retrieval_backend: String,
    #[serde(default)]
    pub retrieval_embedding_account: Option<String>,
    #[serde(default)]
    pub retrieval_embedding_model: Option<String>,
    #[serde(default = "default_context_retrieval_semantic_weight")]
    pub retrieval_semantic_weight: f64,
    #[serde(default = "default_context_retrieval_min_similarity")]
    pub retrieval_min_similarity: f64,
    #[serde(default = "default_context_retrieval_embedding_batch_size")]
    pub retrieval_embedding_batch_size: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_true")]
    pub adaptive_enabled: bool,
    #[serde(default = "default_routing_adaptive_min_samples")]
    pub adaptive_min_samples: u64,
    #[serde(default = "default_routing_adaptive_ewma_alpha")]
    pub adaptive_ewma_alpha: f64,
    #[serde(default = "default_routing_adaptive_latency_target_ms")]
    pub adaptive_latency_target_ms: u64,
    #[serde(default = "default_routing_adaptive_max_penalty")]
    pub adaptive_max_penalty: i32,
    #[serde(default = "default_routing_adaptive_failure_weight")]
    pub adaptive_failure_weight: f64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            adaptive_enabled: true,
            adaptive_min_samples: default_routing_adaptive_min_samples(),
            adaptive_ewma_alpha: default_routing_adaptive_ewma_alpha(),
            adaptive_latency_target_ms: default_routing_adaptive_latency_target_ms(),
            adaptive_max_penalty: default_routing_adaptive_max_penalty(),
            adaptive_failure_weight: default_routing_adaptive_failure_weight(),
        }
    }
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
            retrieval_max_chunks: default_context_retrieval_max_chunks(),
            retrieval_max_tokens: default_context_retrieval_max_tokens(),
            retrieval_min_score: default_context_retrieval_min_score(),
            retrieval_backend: default_context_retrieval_backend(),
            retrieval_embedding_account: None,
            retrieval_embedding_model: None,
            retrieval_semantic_weight: default_context_retrieval_semantic_weight(),
            retrieval_min_similarity: default_context_retrieval_min_similarity(),
            retrieval_embedding_batch_size: default_context_retrieval_embedding_batch_size(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_models_path")]
    pub models_path: String,
}

impl ProviderConfig {
    pub fn is_browser(&self) -> bool {
        self.kind.starts_with("browser-")
    }

    pub fn transport(&self) -> &'static str {
        if self.is_browser() { "browser" } else { "api" }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub provider: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub discover_models: bool,
}

impl AccountConfig {
    pub fn discovery_enabled(&self, provider: &ProviderConfig) -> bool {
        self.discover_models && !provider.is_browser()
    }

    pub fn credential_required(&self, provider: &ProviderConfig) -> bool {
        !provider.is_browser()
    }
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
        let mut config: Self = toml::from_str(&raw)?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    fn normalize(&mut self) {
        let browser_providers = self
            .providers
            .iter()
            .filter(|provider| provider.is_browser())
            .map(|provider| provider.id.clone())
            .collect::<HashSet<_>>();
        for account in &mut self.accounts {
            if browser_providers.contains(&account.provider) {
                account.discover_models = false;
            }
        }
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
        if self.routing.adaptive_min_samples == 0 {
            return Err(ConfigError::Invalid(
                "routing.adaptive_min_samples must be greater than zero".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.routing.adaptive_ewma_alpha)
            || self.routing.adaptive_ewma_alpha == 0.0
            || !self.routing.adaptive_ewma_alpha.is_finite()
        {
            return Err(ConfigError::Invalid(
                "routing.adaptive_ewma_alpha must be greater than 0 and at most 1".into(),
            ));
        }
        if self.routing.adaptive_latency_target_ms == 0 {
            return Err(ConfigError::Invalid(
                "routing.adaptive_latency_target_ms must be greater than zero".into(),
            ));
        }
        if self.routing.adaptive_max_penalty < 0 {
            return Err(ConfigError::Invalid(
                "routing.adaptive_max_penalty must be non-negative".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.routing.adaptive_failure_weight)
            || !self.routing.adaptive_failure_weight.is_finite()
        {
            return Err(ConfigError::Invalid(
                "routing.adaptive_failure_weight must be between 0 and 1".into(),
            ));
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
        if self.context.retrieval_enabled && self.context.retrieval_max_chunks == 0 {
            return Err(ConfigError::Invalid(
                "context.retrieval_max_chunks must be greater than zero when retrieval is enabled"
                    .into(),
            ));
        }
        if self.context.retrieval_enabled && self.context.retrieval_max_tokens < 128 {
            return Err(ConfigError::Invalid(
                "context.retrieval_max_tokens must be at least 128 when retrieval is enabled"
                    .into(),
            ));
        }
        if !self.context.retrieval_min_score.is_finite() || self.context.retrieval_min_score < 0.0 {
            return Err(ConfigError::Invalid(
                "context.retrieval_min_score must be a finite non-negative number".into(),
            ));
        }
        if !matches!(self.context.retrieval_backend.as_str(), "local" | "hybrid") {
            return Err(ConfigError::Invalid(
                "context.retrieval_backend must be 'local' or 'hybrid'".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.context.retrieval_semantic_weight)
            || !self.context.retrieval_semantic_weight.is_finite()
        {
            return Err(ConfigError::Invalid(
                "context.retrieval_semantic_weight must be between 0 and 1".into(),
            ));
        }
        if !(-1.0..=1.0).contains(&self.context.retrieval_min_similarity)
            || !self.context.retrieval_min_similarity.is_finite()
        {
            return Err(ConfigError::Invalid(
                "context.retrieval_min_similarity must be between -1 and 1".into(),
            ));
        }
        if self.context.retrieval_embedding_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "context.retrieval_embedding_batch_size must be greater than zero".into(),
            ));
        }

        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                return Err(ConfigError::Invalid("provider id cannot be empty".into()));
            }
            match provider.kind.as_str() {
                "openai-compatible" | "browser-http" if provider.base_url.trim().is_empty() => {
                    return Err(ConfigError::Invalid(format!(
                        "provider '{}' kind '{}' requires base_url",
                        provider.id, provider.kind
                    )));
                }
                "openai-compatible" | "browser-http" | "browser-cdp" => {}
                other => {
                    return Err(ConfigError::Invalid(format!(
                        "provider '{}' uses unsupported kind '{}'",
                        provider.id, other
                    )));
                }
            }
        }

        for account in &self.accounts {
            let provider = self.provider(&account.provider).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "account '{}' references unknown provider '{}'",
                    account.id, account.provider
                ))
            })?;
            if account.credential_required(provider) && account.api_key_env.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "account '{}' requires api_key_env for provider kind '{}'",
                    account.id, provider.kind
                )));
            }
        }

        if self.context.retrieval_backend == "hybrid" {
            let account_id = self
                .context
                .retrieval_embedding_account
                .as_deref()
                .ok_or_else(|| {
                    ConfigError::Invalid(
                        "context.retrieval_embedding_account is required for hybrid retrieval"
                            .into(),
                    )
                })?;
            let account = self.account(account_id).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "context.retrieval_embedding_account references unknown account '{account_id}'"
                ))
            })?;
            if !account.enabled {
                return Err(ConfigError::Invalid(format!(
                    "context.retrieval_embedding_account '{account_id}' is disabled"
                )));
            }
            let provider = self.provider(&account.provider).ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "context.retrieval_embedding_account '{account_id}' references unknown provider '{}'",
                    account.provider
                ))
            })?;
            if provider.is_browser() {
                return Err(ConfigError::Invalid(format!(
                    "context.retrieval_embedding_account '{account_id}' must use an API provider"
                )));
            }
            if self
                .context
                .retrieval_embedding_model
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(ConfigError::Invalid(
                    "context.retrieval_embedding_model is required for hybrid retrieval".into(),
                ));
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

fn default_host() -> IpAddr {
    "127.0.0.1".parse().expect("valid loopback address")
}
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
fn default_context_retrieval_max_chunks() -> usize { 3 }
fn default_context_retrieval_max_tokens() -> usize { 2_400 }
fn default_context_retrieval_min_score() -> f64 { 0.35 }
fn default_context_retrieval_backend() -> String { "local".into() }
fn default_context_retrieval_semantic_weight() -> f64 { 0.70 }
fn default_context_retrieval_min_similarity() -> f64 { 0.15 }
fn default_context_retrieval_embedding_batch_size() -> usize { 64 }
fn default_routing_adaptive_min_samples() -> u64 { 3 }
fn default_routing_adaptive_ewma_alpha() -> f64 { 0.25 }
fn default_routing_adaptive_latency_target_ms() -> u64 { 1_200 }
fn default_routing_adaptive_max_penalty() -> i32 { 30 }
fn default_routing_adaptive_failure_weight() -> f64 { 0.70 }

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config(provider: &str, account: &str) -> String {
        format!(
            r#"
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[context]
enabled = false
retrieval_enabled = false

{provider}

{account}

[[routes]]
id = "route"
account = "account"
model = "model"
enabled = true

[virtual_models.llmgateway-auto]
routes = ["route"]
"#
        )
    }

    #[test]
    fn wildcard_alias_matches_prefix() {
        assert!(pattern_matches("claude-sonnet-*", "claude-sonnet-4-5"));
        assert!(!pattern_matches("claude-sonnet-*", "claude-opus-4-1"));
    }

    #[test]
    fn browser_account_does_not_require_dummy_credentials_or_discovery_flag() {
        let raw = minimal_config(
            r#"[[providers]]
id = "browser"
kind = "browser-cdp""#,
            r#"[[accounts]]
id = "account"
provider = "browser"
enabled = true"#,
        );
        let mut config: AppConfig = toml::from_str(&raw).unwrap();
        config.normalize();
        config.validate().unwrap();
        let account = config.account("account").unwrap();
        let provider = config.provider("browser").unwrap();
        assert!(account.api_key_env.is_empty());
        assert!(!account.discover_models);
        assert!(!account.discovery_enabled(provider));
        assert!(!account.credential_required(provider));
        assert_eq!(provider.transport(), "browser");
    }

    #[test]
    fn api_account_still_requires_credentials_and_defaults_discovery_on() {
        let raw = minimal_config(
            r#"[[providers]]
id = "api"
kind = "openai-compatible"
base_url = "https://example.test/v1""#,
            r#"[[accounts]]
id = "account"
provider = "api"
api_key_env = "EXAMPLE_API_KEY"
enabled = true"#,
        );
        let mut config: AppConfig = toml::from_str(&raw).unwrap();
        config.normalize();
        config.validate().unwrap();
        let account = config.account("account").unwrap();
        let provider = config.provider("api").unwrap();
        assert!(account.discover_models);
        assert!(account.discovery_enabled(provider));
        assert!(account.credential_required(provider));
        assert_eq!(provider.transport(), "api");
    }

    #[test]
    fn browser_account_explicit_discovery_is_normalized_off() {
        let raw = minimal_config(
            r#"[[providers]]
id = "browser"
kind = "browser-cdp""#,
            r#"[[accounts]]
id = "account"
provider = "browser"
enabled = true
discover_models = true"#,
        );
        let mut config: AppConfig = toml::from_str(&raw).unwrap();
        config.normalize();
        config.validate().unwrap();
        assert!(!config.account("account").unwrap().discover_models);
    }
}
