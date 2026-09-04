use crate::config::AppConfig;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct LiveConfig {
    inner: Arc<RwLock<Arc<AppConfig>>>,
}

impl LiveConfig {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
        }
    }

    pub fn snapshot(&self) -> Arc<AppConfig> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn replace(&self, config: Arc<AppConfig>) -> Arc<AppConfig> {
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *guard, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_complete_immutable_snapshots() {
        let first = Arc::new(AppConfig::parse(
            r#"
[server]
host="127.0.0.1"
port=7331
[api]
key_env="KEY"
default_model="auto"
[[providers]]
id="p"
kind="openai-compatible"
base_url="http://localhost"
[[accounts]]
id="a"
provider="p"
api_key_env="UP"
enabled=true
[[routes]]
id="r"
account="a"
model="m"
enabled=true
[virtual_models.auto]
routes=["r"]
"#,
        ).unwrap());
        let live = LiveConfig::new(first);
        assert_eq!(live.snapshot().server.port, 7331);

        let second = Arc::new(AppConfig::parse(
            r#"
[server]
host="127.0.0.1"
port=7444
[api]
key_env="KEY"
default_model="auto"
[[providers]]
id="p"
kind="openai-compatible"
base_url="http://localhost"
[[accounts]]
id="a"
provider="p"
api_key_env="UP"
enabled=true
[[routes]]
id="r"
account="a"
model="m"
enabled=true
[virtual_models.auto]
routes=["r"]
"#,
        ).unwrap());
        live.replace(second);
        assert_eq!(live.snapshot().server.port, 7444);
    }
}
