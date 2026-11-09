//! Signing secrets at rest: AES-256-GCM with keys from a secret provider.
//!
//! A sealed value looks like `v1:<kid>:<nonce base64>:<ciphertext base64>`. The key id (kid)
//! is also the associated data, so a value cannot be moved to another key id. The keys
//! come from a provider: environment variables `GATEWAY_SECRET_<KID>` or files in a directory.
//! Each holds 32 random bytes in base64. A cloud key service adapter is not built.

use aes_gcm::{
    Aes256Gcm, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use rand::RngExt;
use serde::Deserialize;
use std::path::PathBuf;

const PREFIX: &str = "v1";

/// Where the key bytes come from.
pub trait SecretProvider {
    /// The raw key bytes for a name.
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>>;
}

/// Reads `GATEWAY_SECRET_<NAME>` (name in upper case, other characters as `_`), base64.
pub struct EnvSecretProvider;

impl SecretProvider for EnvSecretProvider {
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        let var = format!(
            "GATEWAY_SECRET_{}",
            name.to_ascii_uppercase()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        );
        let v = std::env::var(&var)
            .with_context(|| format!("environment variable {var} is not set"))?;
        B64.decode(v.trim())
            .with_context(|| format!("{var} is not valid base64"))
    }
}

/// Reads the file `<dir>/<name>`, base64. Fits a mounted secret volume.
pub struct FileSecretProvider(pub PathBuf);

impl SecretProvider for FileSecretProvider {
    fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            bail!("bad key name {name:?}");
        }
        let path = self.0.join(name);
        let v = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read key file {}", path.display()))?;
        B64.decode(v.trim())
            .with_context(|| format!("key file {} is not valid base64", path.display()))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecretsCfg {
    /// `env` or `file`.
    pub provider: String,
    /// Directory for the `file` provider.
    pub dir: Option<String>,
    /// The key id used to seal new values.
    pub active_kid: String,
    /// Key ids that can still open old values.
    pub previous_kids: Vec<String>,
}

impl Default for SecretsCfg {
    fn default() -> Self {
        Self {
            provider: "env".into(),
            dir: None,
            active_kid: "primary".into(),
            previous_kids: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Keyring {
    active: (String, Key<Aes256Gcm>),
    previous: Vec<(String, Key<Aes256Gcm>)>,
}

fn to_key(bytes: &[u8], kid: &str) -> anyhow::Result<Key<Aes256Gcm>> {
    if bytes.len() != 32 {
        bail!("key {kid} must be 32 bytes, got {}", bytes.len());
    }
    Ok(*Key::<Aes256Gcm>::from_slice(bytes))
}

impl Keyring {
    pub fn new(active: (&str, &[u8]), previous: &[(&str, &[u8])]) -> anyhow::Result<Self> {
        Ok(Self {
            active: (active.0.to_string(), to_key(active.1, active.0)?),
            previous: previous
                .iter()
                .map(|(k, b)| Ok((k.to_string(), to_key(b, k)?)))
                .collect::<anyhow::Result<_>>()?,
        })
    }

    /// Load the keys named in the config. A missing active key is an error.
    pub fn load(cfg: &SecretsCfg) -> anyhow::Result<Self> {
        let provider: Box<dyn SecretProvider> = match cfg.provider.as_str() {
            "env" => Box::new(EnvSecretProvider),
            "file" => Box::new(FileSecretProvider(PathBuf::from(
                cfg.dir
                    .clone()
                    .ok_or_else(|| anyhow!("secrets.dir is needed for the file provider"))?,
            ))),
            other => bail!("unknown secrets.provider {other:?} (use env or file)"),
        };
        let active = provider
            .get(&cfg.active_kid)
            .with_context(|| format!("no active signing-secret key {:?}", cfg.active_kid))?;
        let mut previous = Vec::new();
        for kid in &cfg.previous_kids {
            previous.push((kid.clone(), to_key(&provider.get(kid)?, kid)?));
        }
        Ok(Self {
            active: (cfg.active_kid.clone(), to_key(&active, &cfg.active_kid)?),
            previous,
        })
    }

    pub fn active_kid(&self) -> &str {
        &self.active.0
    }

    fn key_for(&self, kid: &str) -> Option<&Key<Aes256Gcm>> {
        if self.active.0 == kid {
            return Some(&self.active.1);
        }
        self.previous
            .iter()
            .find(|(k, _)| k == kid)
            .map(|(_, key)| key)
    }

    /// Seal with the active key.
    pub fn seal(&self, plain: &str) -> String {
        self.seal_with(&self.active.0, &self.active.1, plain)
    }

    fn seal_with(&self, kid: &str, key: &Key<Aes256Gcm>, plain: &str) -> String {
        let nonce_bytes: [u8; 12] = rand::rng().random();
        let ct = Aes256Gcm::new(key)
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plain.as_bytes(),
                    aad: kid.as_bytes(),
                },
            )
            .expect("AES-GCM encryption does not fail for short inputs");
        format!(
            "{PREFIX}:{kid}:{}:{}",
            B64.encode(nonce_bytes),
            B64.encode(ct)
        )
    }

    /// The key id of a sealed value, or None for a plain legacy value.
    pub fn kid_of(sealed: &str) -> Option<&str> {
        let mut parts = sealed.splitn(4, ':');
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(PREFIX), Some(kid), Some(_), Some(_)) => Some(kid),
            _ => None,
        }
    }

    /// Open a sealed value. A value without the `v1:` format is a legacy plain secret
    /// from before sealing existed. It is returned as it is, and `reseal-secrets` replaces it.
    pub fn open(&self, stored: &str) -> anyhow::Result<String> {
        let mut parts = stored.splitn(4, ':');
        let (Some(PREFIX), Some(kid), Some(nonce), Some(ct)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Ok(stored.to_string());
        };
        let key = self
            .key_for(kid)
            .ok_or_else(|| anyhow!("unknown key id {kid:?}"))?;
        let nonce = B64.decode(nonce).context("bad nonce")?;
        if nonce.len() != 12 {
            bail!("bad nonce length");
        }
        let ct = B64.decode(ct).context("bad ciphertext")?;
        let plain = Aes256Gcm::new(key)
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ct,
                    aad: kid.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("cannot open the secret (wrong key or changed data)"))?;
        String::from_utf8(plain).context("secret is not UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> Keyring {
        Keyring::new(("k2", &[2u8; 32]), &[("k1", &[1u8; 32])]).unwrap()
    }

    #[test]
    fn round_trip_and_format() {
        let r = ring();
        let s = r.seal("whsec_abc");
        assert!(s.starts_with("v1:k2:"));
        assert!(!s.contains("whsec_abc"));
        assert_eq!(r.open(&s).unwrap(), "whsec_abc");
    }

    #[test]
    fn same_secret_seals_differently_each_time() {
        let r = ring();
        assert_ne!(r.seal("x"), r.seal("x"));
    }

    #[test]
    fn tamper_is_detected() {
        let r = ring();
        let mut s = r.seal("whsec_abc");
        let last = s.pop().unwrap();
        s.push(if last == 'A' { 'B' } else { 'A' });
        assert!(r.open(&s).is_err());
    }

    #[test]
    fn unknown_kid_is_an_error() {
        let other = Keyring::new(("zz", &[9u8; 32]), &[]).unwrap();
        let s = other.seal("x");
        assert!(
            ring()
                .open(&s)
                .unwrap_err()
                .to_string()
                .contains("unknown key id")
        );
    }

    #[test]
    fn kid_is_bound_to_the_data() {
        let r = ring();
        let s = r.seal("x");
        let moved = s.replacen("v1:k2:", "v1:k1:", 1);
        assert!(r.open(&moved).is_err());
    }

    #[test]
    fn previous_key_still_opens() {
        let old = Keyring::new(("k1", &[1u8; 32]), &[]).unwrap();
        let sealed = old.seal("whsec_old");
        assert_eq!(ring().open(&sealed).unwrap(), "whsec_old");
    }

    #[test]
    fn legacy_plain_value_passes_through() {
        assert_eq!(ring().open("whsec_plain").unwrap(), "whsec_plain");
        assert_eq!(Keyring::kid_of("whsec_plain"), None);
    }

    #[test]
    fn wrong_length_key_is_refused() {
        assert!(Keyring::new(("k", &[0u8; 16]), &[]).is_err());
    }
}
