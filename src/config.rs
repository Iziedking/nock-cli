use std::env;

use thiserror::Error;
use zeroize::Zeroizing;

/// Robinhood Chain.
pub const CHAIN_ID: u64 = 4663;
pub const DEFAULT_RPC: &str = "https://rpc.mainnet.chain.robinhood.com";
pub const DEFAULT_SEQUENCER: &str = "https://sequencer.mainnet.chain.robinhood.com";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("NOCK_PRIVATE_KEY is not 32 bytes of hex")]
    InvalidPrivateKey,
    #[error("NOCK_RPC_URLS is set but empty")]
    NoRpcUrls,
}

/// Everything the CLI reads from the environment.
///
/// Deliberately not from flags. A private key on a command line reaches shell
/// history and process listings, and is visible to every other process on the
/// machine for as long as the command runs.
pub struct Config {
    pub chain_id: u64,
    /// Read endpoints, tried in order. The first is preferred.
    pub rpc_urls: Vec<String>,
    /// Send-only. Answers -32601 to everything but `eth_sendRawTransaction`.
    pub sequencer_url: String,
    private_key: Option<Zeroizing<String>>,
}

// The key must never reach a log, a panic message or a bug report, and the
// easiest way to leak one is a derived Debug. This prints whether a key is
// present and nothing about what it is.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("chain_id", &self.chain_id)
            .field("rpc_urls", &self.rpc_urls)
            .field("sequencer_url", &self.sequencer_url)
            .field("private_key", &self.private_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let rpc_urls = match env::var("NOCK_RPC_URLS") {
            Ok(raw) => {
                let urls: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                if urls.is_empty() {
                    return Err(ConfigError::NoRpcUrls);
                }
                urls
            }
            Err(_) => vec![DEFAULT_RPC.to_owned()],
        };

        let private_key = match env::var("NOCK_PRIVATE_KEY") {
            Ok(raw) => {
                let key = Zeroizing::new(raw.trim().to_owned());
                validate_private_key(&key)?;
                Some(key)
            }
            Err(_) => None,
        };

        Ok(Self {
            chain_id: env::var("NOCK_CHAIN_ID")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(CHAIN_ID),
            rpc_urls,
            sequencer_url: env::var("NOCK_SEQUENCER_URL")
                .unwrap_or_else(|_| DEFAULT_SEQUENCER.to_owned()),
            private_key,
        })
    }

    /// The signing key, if one is configured. Handed out inside `Zeroizing` so a
    /// caller cannot accidentally keep a plain copy alive.
    #[must_use]
    pub fn private_key(&self) -> Option<&Zeroizing<String>> {
        self.private_key.as_ref()
    }

    // Used by the fire path, which lands in P5. Kept here now because the
    // ordering it encodes is a decision, not an implementation detail.
    #[allow(dead_code)]
    /// Every endpoint a transaction should be pushed to: the sequencer, which is
    /// the only one that orders anything, then the read endpoints as additional
    /// ways in.
    #[must_use]
    pub fn send_urls(&self) -> Vec<String> {
        let mut urls = vec![self.sequencer_url.clone()];
        urls.extend(self.rpc_urls.iter().cloned());
        urls
    }
}

fn validate_private_key(value: &str) -> Result<(), ConfigError> {
    let body = value.strip_prefix("0x").unwrap_or(value);
    if body.len() != 64 {
        return Err(ConfigError::InvalidPrivateKey);
    }
    // Decoded into a Zeroizing buffer and dropped immediately: this only checks
    // the shape, so the bytes must not outlive the check.
    let decoded = Zeroizing::new(hex::decode(body).map_err(|_| ConfigError::InvalidPrivateKey)?);
    if decoded.len() == 32 {
        Ok(())
    } else {
        Err(ConfigError::InvalidPrivateKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_key_with_or_without_the_prefix() {
        let body = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        assert!(validate_private_key(body).is_ok());
        assert!(validate_private_key(&format!("0x{body}")).is_ok());
    }

    #[test]
    fn refuses_anything_that_is_not_32_bytes_of_hex() {
        assert!(validate_private_key("0x00").is_err());
        assert!(validate_private_key(&"z".repeat(64)).is_err());
        assert!(validate_private_key("").is_err());
    }

    /// The one test that matters here. A key in a debug dump is a key in a bug
    /// report, and from there in a chat window.
    #[test]
    fn debug_output_never_contains_the_private_key() {
        let secret = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let config = Config {
            chain_id: CHAIN_ID,
            rpc_urls: vec![DEFAULT_RPC.to_owned()],
            sequencer_url: DEFAULT_SEQUENCER.to_owned(),
            private_key: Some(Zeroizing::new(format!("0x{secret}"))),
        };
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(secret),
            "the key leaked into Debug output"
        );
        assert!(rendered.contains("<set>"));
    }

    #[test]
    fn the_sequencer_is_the_first_place_a_transaction_goes() {
        let config = Config {
            chain_id: CHAIN_ID,
            rpc_urls: vec!["https://read.example".to_owned()],
            sequencer_url: DEFAULT_SEQUENCER.to_owned(),
            private_key: None,
        };
        let urls = config.send_urls();
        assert_eq!(urls[0], DEFAULT_SEQUENCER);
        assert_eq!(urls.len(), 2);
    }
}
