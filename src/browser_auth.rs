use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use uuid::Uuid;

const VAULT_VERSION: u32 = 1;
const KEY_FILE_NAME: &str = ".vault-key";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BrowserAuthCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    #[serde(default)]
    pub expires: f64,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub same_site: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BrowserAuthMaterial {
    pub version: u32,
    pub session_id: String,
    pub provider: String,
    pub source_url: String,
    pub user_agent: String,
    pub cookies: Vec<BrowserAuthCookie>,
    #[serde(default)]
    pub local_storage: BTreeMap<String, String>,
    #[serde(default)]
    pub session_storage: BTreeMap<String, String>,
    pub captured_at: String,
}

impl BrowserAuthMaterial {
    pub fn new(
        session_id: impl Into<String>,
        provider: impl Into<String>,
        source_url: impl Into<String>,
        user_agent: impl Into<String>,
        cookies: Vec<BrowserAuthCookie>,
        local_storage: BTreeMap<String, String>,
        session_storage: BTreeMap<String, String>,
    ) -> Self {
        Self {
            version: VAULT_VERSION,
            session_id: session_id.into(),
            provider: provider.into(),
            source_url: source_url.into(),
            user_agent: user_agent.into(),
            cookies,
            local_storage,
            session_storage,
            captured_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }

    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .filter(|cookie| !cookie.name.trim().is_empty())
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn cookie_header_for_host(&self, host: &str) -> String {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.cookies
            .iter()
            .filter(|cookie| {
                if cookie.name.trim().is_empty() {
                    return false;
                }
                let domain = cookie
                    .domain
                    .trim()
                    .trim_start_matches('.')
                    .trim_end_matches('.')
                    .to_ascii_lowercase();
                !domain.is_empty()
                    && (host == domain || host.ends_with(&format!(".{domain}")))
            })
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn cookie_value_for_host(&self, host: &str, name: &str) -> Option<&str> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        self.cookies.iter().find_map(|cookie| {
            let domain = cookie
                .domain
                .trim()
                .trim_start_matches('.')
                .trim_end_matches('.')
                .to_ascii_lowercase();
            if cookie.name == name
                && !domain.is_empty()
                && (host == domain || host.ends_with(&format!(".{domain}")))
            {
                Some(cookie.value.as_str())
            } else {
                None
            }
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SealedMaterial {
    version: u32,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone)]
pub struct BrowserAuthVault {
    root: PathBuf,
    cipher: Arc<Aes256Gcm>,
}

#[derive(Debug, Error)]
pub enum BrowserAuthVaultError {
    #[error("browser auth vault I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("browser auth vault serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("browser auth vault key is invalid: {0}")]
    InvalidKey(String),
    #[error("browser auth vault material for session '{0}' was not found")]
    NotFound(String),
    #[error("browser auth vault encryption failed")]
    Encrypt,
    #[error("browser auth vault decryption failed")]
    Decrypt,
    #[error("browser auth vault entry is invalid: {0}")]
    InvalidEntry(String),
}

impl BrowserAuthVault {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, BrowserAuthVaultError> {
        let root = root.as_ref().to_path_buf();
        ensure_private_dir(&root)?;
        let key = load_or_create_key(&root.join(KEY_FILE_NAME))?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|_| BrowserAuthVaultError::InvalidKey("expected 32 bytes".into()))?;
        Ok(Self {
            root,
            cipher: Arc::new(cipher),
        })
    }

     pub fn contains(&self, session_id: &str) -> bool {
        self.entry_path(session_id)
            .is_ok_and(|path| path.is_file())
    }

    pub fn store(&self, material: &BrowserAuthMaterial) -> Result<(), BrowserAuthVaultError> {
        validate_session_id(&material.session_id)?;
        if material.version != VAULT_VERSION {
            return Err(BrowserAuthVaultError::InvalidEntry(format!(
                "unsupported material version {}",
                material.version
            )));
        }

        let plaintext = serde_json::to_vec(material)?;
        let nonce_bytes = random_nonce();
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_ref())
            .map_err(|_| BrowserAuthVaultError::Encrypt)?;
        let sealed = SealedMaterial {
            version: VAULT_VERSION,
            nonce: STANDARD.encode(nonce_bytes),
            ciphertext: STANDARD.encode(ciphertext),
        };
        let rendered = serde_json::to_vec(&sealed)?;
        atomic_private_write(&self.entry_path(&material.session_id)?, &rendered)
    }

    pub fn load(&self, session_id: &str) -> Result<BrowserAuthMaterial, BrowserAuthVaultError> {
        let path = self.entry_path(session_id)?;
        if !path.is_file() {
            return Err(BrowserAuthVaultError::NotFound(session_id.to_string()));
        }
        let sealed: SealedMaterial = serde_json::from_slice(&fs::read(path)?)?;
        if sealed.version != VAULT_VERSION {
            return Err(BrowserAuthVaultError::InvalidEntry(format!(
                "unsupported sealed version {}",
                sealed.version
            )));
        }
        let nonce = STANDARD
            .decode(sealed.nonce)
            .map_err(|error| BrowserAuthVaultError::InvalidEntry(error.to_string()))?;
        if nonce.len() != 12 {
            return Err(BrowserAuthVaultError::InvalidEntry(
                "AES-GCM nonce must be 12 bytes".into(),
            ));
        }
        let ciphertext = STANDARD
            .decode(sealed.ciphertext)
            .map_err(|error| BrowserAuthVaultError::InvalidEntry(error.to_string()))?;
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| BrowserAuthVaultError::Decrypt)?;
        let material: BrowserAuthMaterial = serde_json::from_slice(&plaintext)?;
        if material.session_id != session_id {
            return Err(BrowserAuthVaultError::InvalidEntry(
                "session id does not match sealed material".into(),
            ));
        }
        Ok(material)
    }

     fn entry_path(&self, session_id: &str) -> Result<PathBuf, BrowserAuthVaultError> {
        validate_session_id(session_id)?;
        Ok(self.root.join(format!("{session_id}.auth")))
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32], BrowserAuthVaultError> {
    if path.is_file() {
        let encoded = fs::read_to_string(path)?;
        let decoded = STANDARD
            .decode(encoded.trim())
            .map_err(|error| BrowserAuthVaultError::InvalidKey(error.to_string()))?;
        return decoded
            .try_into()
            .map_err(|_| BrowserAuthVaultError::InvalidKey("expected 32 decoded bytes".into()));
    }

    let first = *Uuid::new_v4().as_bytes();
    let second = *Uuid::new_v4().as_bytes();
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&first);
    key[16..].copy_from_slice(&second);
    atomic_private_write(path, STANDARD.encode(key).as_bytes())?;
    Ok(key)
}

fn random_nonce() -> [u8; 12] {
    let bytes = *Uuid::new_v4().as_bytes();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&bytes[..12]);
    nonce
}

fn validate_session_id(session_id: &str) -> Result<(), BrowserAuthVaultError> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(BrowserAuthVaultError::InvalidEntry(
            "session id may contain only letters, numbers, '.', '-' and '_'".into(),
        ));
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_private_write(path: &Path, contents: &[u8]) -> Result<(), BrowserAuthVaultError> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    fs::write(&temp, contents)?;
    set_private_file_permissions(&temp)?;
    if let Err(error) = fs::rename(&temp, path) {
        if error.kind() == std::io::ErrorKind::AlreadyExists
            || error.kind() == std::io::ErrorKind::PermissionDenied
        {
            fs::copy(&temp, path)?;
            let _ = fs::remove_file(&temp);
        } else {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    set_private_file_permissions(path)?;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "llmgateway-browser-auth-vault-{}",
            Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn round_trips_sealed_auth_material() {
        let root = temp_root();
        let vault = BrowserAuthVault::open(&root).unwrap();
        let material = BrowserAuthMaterial::new(
            "gemini-web-one",
            "gemini-web",
            "https://gemini.google.com/app/example",
            "test-agent",
            vec![BrowserAuthCookie {
                name: "SID".into(),
                value: "secret".into(),
                domain: ".google.com".into(),
                path: "/".into(),
                expires: 0.0,
                http_only: true,
                secure: true,
                same_site: Some("Lax".into()),
            }],
            BTreeMap::from([("theme".into(), "dark".into())]),
            BTreeMap::new(),
        );

        vault.store(&material).unwrap();
        assert!(vault.contains("gemini-web-one"));
        let restored = vault.load("gemini-web-one").unwrap();
        assert_eq!(restored, material);
        assert_eq!(restored.cookie_header(), "SID=secret");
        assert_eq!(restored.cookie_header_for_host("gemini.google.com"), "SID=secret");
        assert_eq!(
            restored.cookie_value_for_host("gemini.google.com", "SID"),
            Some("secret")
        );
        assert!(restored.cookie_header_for_host("chatgpt.com").is_empty());

        let raw = fs::read_to_string(root.join("gemini-web-one.auth")).unwrap();
        assert!(!raw.contains("secret"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_path_traversal_session_ids() {
        let root = temp_root();
        let vault = BrowserAuthVault::open(&root).unwrap();
        assert!(vault.load("../escape").is_err());
        let _ = fs::remove_dir_all(root);
    }
}
