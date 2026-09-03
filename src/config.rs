use serde::Deserialize;
use std::{collections::HashMap, env, fs, net::IpAddr, path::Path};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub api: ApiConfig,
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
pub struct ProviderConfig {
    pub id: String,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    pub base_url: String,
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

fn default_host() -> IpAddr {
    "127.0.0.1".parse().expect("valid loopback address")
}
fn default_port() -> u16 { 7331 }
fn default_gateway_key_env() -> String { "LLMGATEWAY_API_KEY".into() }
fn default_model() -> String { "llmgateway-auto".into() }
fn default_provider_kind() -> String { "openai-compatible".into() }
fn default_auth_style() -> String { "bearer".into() }
fn default_priority() -> i32 { 100 }
fn default_true() -> bool { true }

#[cfg(test)]
mod tests {
    use super::pattern_matches;

    #[test]
    fn wildcard_alias_matches_prefix() {
        assert!(pattern_matches("claude-sonnet-*", "claude-sonnet-4-5"));
        assert!(!pattern_matches("claude-sonnet-*", "claude-opus-4-1"));
    }
}
