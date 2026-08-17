use std::time::{Duration, Instant};

use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    /// The node answered, and the answer was no. Failing over would produce the
    /// identical answer from the next endpoint, so this is never retried.
    #[error("{message}")]
    Rejected { code: i64, message: String },
    /// Nobody answered. This one is worth trying elsewhere.
    #[error("every endpoint failed: {0}")]
    AllFailed(String),
    #[error("could not read the response: {0}")]
    Malformed(String),
}

/// A JSON-RPC client that fails over between endpoints and remembers which one
/// last worked.
///
/// An Alchemy style endpoint carries its key in the URL path, so a URL inside an
/// error message is a secret in a log file. Everything printed here goes through
/// `redact`.
#[derive(Debug)]
pub struct Rpc {
    client: Client,
    urls: Vec<String>,
    preferred: usize,
}

impl Rpc {
    #[must_use]
    pub fn new(urls: Vec<String>, timeout: Duration) -> Self {
        Self {
            client: Client::builder()
                .timeout(timeout)
                // Keep the pool alive between calls. Preparation warms the
                // connection that the fire path then reuses; a cold connection
                // costs three round trips instead of one.
                .pool_idle_timeout(None)
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_mins(1))
                .build()
                .expect("a client with no TLS backend cannot be built"),
            urls,
            preferred: 0,
        }
    }

    #[must_use]
    pub fn endpoint(&self) -> String {
        redact(self.urls.get(self.preferred).map_or("", String::as_str))
    }

    pub async fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let order: Vec<usize> = std::iter::once(self.preferred)
            .chain((0..self.urls.len()).filter(|i| *i != self.preferred))
            .collect();

        let mut problems = Vec::new();
        for index in order {
            let url = &self.urls[index];
            match self.attempt(url, method, &params).await {
                Ok(value) => {
                    self.preferred = index;
                    return serde_json::from_value(value)
                        .map_err(|e| RpcError::Malformed(e.to_string()));
                }
                // An answer, even a negative one, is not a transport failure.
                Err(rejected @ RpcError::Rejected { .. }) => return Err(rejected),
                Err(other) => problems.push(format!("{}: {other}", redact(url))),
            }
        }
        Err(RpcError::AllFailed(problems.join("; ")))
    }

    async fn attempt(&self, url: &str, method: &str, params: &Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| RpcError::AllFailed(e.to_string()))?;

        if !response.status().is_success() {
            return Err(RpcError::AllFailed(format!("HTTP {}", response.status())));
        }

        let parsed: Value = response
            .json()
            .await
            .map_err(|e| RpcError::Malformed(e.to_string()))?;

        if let Some(error) = parsed.get("error") {
            return Err(RpcError::Rejected {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
                    .to_owned(),
            });
        }

        parsed
            .get("result")
            .cloned()
            .ok_or_else(|| RpcError::Malformed("no result field".to_owned()))
    }

    /// Round trip to the preferred endpoint, for `doctor`.
    pub async fn timed_call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> (Result<T, RpcError>, Duration) {
        let started = Instant::now();
        let out = self.call(method, params).await;
        (out, started.elapsed())
    }
}

/// Strips path segments long enough to be an API key. Errors are read far more
/// often than they are anticipated, so this happens here rather than at each
/// call site.
#[must_use]
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "an endpoint".to_owned();
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if path.is_empty() {
        return format!("{scheme}://{host}");
    }
    let cleaned: Vec<&str> = path
        .split('/')
        .map(|segment| if segment.len() >= 8 { "…" } else { segment })
        .collect();
    format!("{scheme}://{host}/{}", cleaned.join("/"))
}

/// Hex quantity to `u64`, for the `0x…` numbers every Ethereum RPC returns.
pub fn parse_hex_u64(value: &str) -> Result<u64, RpcError> {
    u64::from_str_radix(value.trim_start_matches("0x"), 16)
        .map_err(|_| RpcError::Malformed(format!("not a hex quantity: {value}")))
}

/// Hex quantity to `u128`, wide enough for any wei balance we will see.
pub fn parse_hex_u128(value: &str) -> Result<u128, RpcError> {
    u128::from_str_radix(value.trim_start_matches("0x"), 16)
        .map_err(|_| RpcError::Malformed(format!("not a hex quantity: {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_a_key_carried_in_the_path() {
        assert_eq!(
            redact("https://robinhood-mainnet.g.alchemy.com/v2/alch_supersecretkey"),
            "https://robinhood-mainnet.g.alchemy.com/v2/…"
        );
    }

    #[test]
    fn leaves_a_plain_endpoint_readable() {
        assert_eq!(
            redact("https://rpc.mainnet.chain.robinhood.com"),
            "https://rpc.mainnet.chain.robinhood.com"
        );
    }

    #[test]
    fn never_echoes_something_it_cannot_parse() {
        assert_eq!(redact("not a url at all"), "an endpoint");
    }

    #[test]
    fn reads_hex_quantities() {
        assert_eq!(parse_hex_u64("0x1234").unwrap(), 0x1234);
        assert_eq!(parse_hex_u64("1234").unwrap(), 0x1234);
        assert!(parse_hex_u64("0xzz").is_err());
        assert_eq!(parse_hex_u128("0xde0b6b3a7640000").unwrap(), 1_000_000_000_000_000_000);
    }
}
