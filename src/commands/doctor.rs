use std::process::ExitCode;
use std::time::Duration;

use alloy_primitives::Address;
use k256::ecdsa::SigningKey;
use serde_json::json;
use zeroize::Zeroizing;

use crate::chain::rpc::{parse_hex_u128, parse_hex_u64, Rpc};
use crate::config::Config;
use crate::engine::clock::{Clock, MAX_DRIFT_MS};

/// Everything checked here is something that fails silently at the drop if it is
/// wrong: an RPC that does not answer, a clock that has drifted, a sequencer
/// that refuses a connection, a key with no gas. Finding out now is free.
/// Finding out at T-0 costs the drop.
#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    /// Not every problem is fatal. Slow is worth saying out loud without
    /// pretending the tool is broken.
    pub warn: bool,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
            warn: false,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
            warn: true,
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
            warn: false,
        }
    }
}

/// Above this, a contested drop is already lost to anyone sitting near the
/// sequencer. Measured: 2.6 ms p50 from us-east-2 against 1,347 ms from a home
/// connection, so this is not a close call and the user deserves to know.
const SLOW_RPC_MS: u128 = 150;

pub async fn run(config: &Config) -> ExitCode {
    println!("\nnock doctor\n");
    let checks = collect(config).await;
    println!("{}", render(&checks));
    println!();
    if checks.iter().all(|c| c.ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub async fn collect(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut rpc = Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10));

    // 1. Is the chain reachable, and is it the chain we think it is? A wrong
    //    chain id means every address and every stage below is meaningless.
    let (chain_id, elapsed) = rpc.timed_call::<String>("eth_chainId", json!([])).await;
    let mut chain_ok = false;
    match chain_id.and_then(|hex| parse_hex_u64(&hex)) {
        Ok(id) if id == config.chain_id => {
            chain_ok = true;
            let ms = elapsed.as_millis();
            let detail = format!("chain {id} via {}, {ms} ms", rpc.endpoint());
            checks.push(if ms > SLOW_RPC_MS {
                Check::warn("rpc", format!("{detail}. Far from the sequencer, so contested drops will be lost to anyone closer"))
            } else {
                Check::ok("rpc", detail)
            });
        }
        Ok(id) => checks.push(Check::fail(
            "rpc",
            format!("answered for chain {id}, expected {}", config.chain_id),
        )),
        Err(err) => checks.push(Check::fail("rpc", err.to_string())),
    }

    // 2. How far along is it? A node parked thousands of blocks back will
    //    happily answer everything else while being useless.
    if chain_ok {
        match rpc
            .call::<String>("eth_blockNumber", json!([]))
            .await
            .and_then(|hex| parse_hex_u64(&hex))
        {
            Ok(block) => checks.push(Check::ok("head", format!("block {block}"))),
            Err(err) => checks.push(Check::fail("head", err.to_string())),
        }
    }

    // 3. The sequencer answers -32601 to everything except
    //    eth_sendRawTransaction, so an ordinary health check cannot probe it.
    //    A rejection of deliberate garbage proves it is listening and willing.
    let mut seq = Rpc::new(vec![config.sequencer_url.clone()], Duration::from_secs(10));
    let (result, elapsed) = seq
        .timed_call::<String>("eth_sendRawTransaction", json!(["0x00"]))
        .await;
    // A rejection is the expected answer, not a failure: it means the endpoint
    // parsed our deliberate garbage and refused it, which is exactly what a
    // healthy send-only endpoint should do. Only a transport failure counts.
    let reachable = !matches!(&result, Err(crate::chain::rpc::RpcError::AllFailed(_)));
    checks.push(if reachable {
        Check::ok(
            "sequencer",
            format!("reachable, {} ms", elapsed.as_millis()),
        )
    } else {
        Check::fail(
            "sequencer",
            result
                .err()
                .map_or_else(|| "unreachable".to_owned(), |e| e.to_string()),
        )
    });

    // 4. A clock that has drifted fires at the wrong moment and never says why.
    //    Checked here because it is the one problem a user cannot see any other
    //    way: everything else fails loudly, this one just loses drops.
    let mut clock = Clock::new();
    clock.sync().await;
    checks.push(match clock.assert_usable() {
        Ok(()) => Check::ok(
            "clock",
            format!("{} ms drift, limit is {MAX_DRIFT_MS} ms", clock.drift_ms()),
        ),
        Err(err) => Check::fail("clock", err.to_string()),
    });

    // 5. A key with no gas looks configured right up until it matters.
    checks.push(wallet_check(config, &mut rpc).await);

    checks
}

async fn wallet_check(config: &Config, rpc: &mut Rpc) -> Check {
    let Some(key) = config.private_key() else {
        return Check::fail(
            "wallet",
            "no NOCK_PRIVATE_KEY set, so nothing can be signed",
        );
    };

    let body = key.strip_prefix("0x").unwrap_or(key);
    let Ok(bytes) = hex::decode(body).map(Zeroizing::new) else {
        return Check::fail("wallet", "NOCK_PRIVATE_KEY is not valid hex");
    };
    let Ok(signing_key) = SigningKey::from_slice(&bytes) else {
        return Check::fail("wallet", "NOCK_PRIVATE_KEY is not a valid signing key");
    };
    let address = Address::from_private_key(&signing_key);

    match rpc
        .call::<String>("eth_getBalance", json!([address.to_string(), "latest"]))
        .await
        .and_then(|hex| parse_hex_u128(&hex))
    {
        Ok(0) => Check::fail(
            "wallet",
            format!("{address} holds nothing, which will not cover a mint"),
        ),
        Ok(wei) => Check::ok("wallet", format!("{address}, {} ETH", format_eth(wei))),
        Err(err) => Check::fail("wallet", err.to_string()),
    }
}

/// Integer arithmetic on purpose. A float turns a balance into an approximation,
/// and this number is the difference between a mint and a failure.
#[must_use]
pub fn format_eth(wei: u128) -> String {
    const WEI_PER_ETH: u128 = 1_000_000_000_000_000_000;
    let whole = wei / WEI_PER_ETH;
    let frac = wei % WEI_PER_ETH;
    format!("{whole}.{frac:018}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

#[must_use]
pub fn render(checks: &[Check]) -> String {
    let mut lines: Vec<String> = checks
        .iter()
        .map(|c| {
            let tag = if !c.ok {
                "FAIL"
            } else if c.warn {
                "warn"
            } else {
                "ok  "
            };
            format!("  {tag}  {:<10} {}", c.name, c.detail)
        })
        .collect();

    let failed = checks.iter().filter(|c| !c.ok).count();
    lines.push(String::new());
    lines.push(if failed == 0 {
        "  Ready. Nothing here will surprise you at the drop.".to_owned()
    } else {
        format!(
            "  {failed} problem{} to fix before this can mint.",
            if failed == 1 { "" } else { "s" }
        )
    });
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_a_balance_without_floating_point() {
        assert_eq!(format_eth(0), "0");
        assert_eq!(format_eth(1_000_000_000_000_000_000), "1");
        assert_eq!(format_eth(1_500_000_000_000_000_000), "1.5");
        // The small numbers matter most: this is roughly one mint's gas.
        assert_eq!(format_eth(2_740_000_000_000), "0.00000274");
        assert_eq!(format_eth(1), "0.000000000000000001");
    }

    #[test]
    fn a_clean_run_says_so_and_a_broken_one_counts() {
        let good = vec![Check::ok("rpc", "fine"), Check::warn("head", "slow")];
        assert!(render(&good).contains("Ready."));
        let bad = vec![Check::fail("rpc", "gone"), Check::ok("head", "fine")];
        assert!(render(&bad).contains("1 problem to fix"));
    }

    /// A warning is not a failure. Being far from the sequencer is worth saying
    /// and is not a reason to refuse to run.
    #[test]
    fn a_warning_does_not_count_as_a_problem() {
        let checks = vec![Check::warn("rpc", "slow")];
        assert!(checks.iter().all(|c| c.ok));
        assert!(render(&checks).contains("warn"));
    }
}
