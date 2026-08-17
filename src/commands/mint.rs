use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use alloy_primitives::Address;
use serde_json::json;
use zeroize::Zeroizing;

use crate::chain::rpc::{parse_hex_u128, parse_hex_u64, Rpc, RpcError};
use crate::chain::seadrop::{
    fee_recipient, mint_public_calldata, public_drop, PublicDrop, SEADROP,
};
use crate::chain::tx::{Eip1559, Signed};
use crate::commands::doctor::format_eth;
use crate::config::Config;
use crate::engine::clock::Clock;
use crate::engine::confirm::{classify, ChainProbe, ConfirmSettings, Outcome};
use crate::wallet::keystore::Keystore;

/// Observed on chain: about 102,000 for one `mintPublic`. Rounded up, because a
/// limit that is too tight fails the mint outright while one that is slightly
/// loose costs nothing when the call succeeds.
const GAS_LIMIT: u64 = 320_000;

/// Everything expensive is done before the stage opens. What remains at the
/// moment of firing is one write per endpoint, so preparation finishing late is
/// worth saying out loud.
const READY_BY_SECONDS: i64 = 30;

pub struct MintArgs<'a> {
    pub collection: &'a str,
    pub quantity: u64,
    pub wallet: &'a Path,
    /// Without this nothing is sent. A command that spends money should not do
    /// so because somebody pressed up-arrow and enter.
    pub fire: bool,
}

pub async fn run(config: &Config, args: MintArgs<'_>) -> ExitCode {
    match prepare_and_run(config, args).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("\n  {message}\n");
            ExitCode::FAILURE
        }
    }
}

async fn prepare_and_run(config: &Config, args: MintArgs<'_>) -> Result<ExitCode, String> {
    let collection: Address = args.collection.trim().parse().map_err(|_| {
        format!(
            "'{}' is not a contract address. This command wants the NFT contract; \
             resolving a name or an OpenSea link is not built yet.",
            args.collection
        )
    })?;

    let mut rpc = Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10));

    // 1. The stage, read from the chain. Nothing about the mint comes from an
    //    API, so nothing about the mint can be delayed by one.
    let drop = public_drop(&mut rpc, collection)
        .await
        .map_err(|e| format!("could not read the stage: {e}"))?;
    let fee = fee_recipient(&mut rpc, collection)
        .await
        .map_err(|e| format!("could not read the fee recipient: {e}"))?;

    if !drop.is_free() {
        return Err(format!(
            "This stage costs {} ETH per mint. Paid mints are not built yet.",
            format_eth(drop.mint_price_wei)
        ));
    }
    if args.quantity == 0 || args.quantity > u64::from(drop.max_per_wallet) {
        return Err(format!(
            "This stage allows {} per wallet and you asked for {}.",
            drop.max_per_wallet, args.quantity
        ));
    }

    // 2. The wallet. Unlocked once, here, and the key is wiped when this scope
    //    ends whatever happens after.
    let store = Keystore::load(args.wallet).map_err(|e| {
        format!(
            "could not read the wallet at {}: {e}",
            args.wallet.display()
        )
    })?;
    let secret = unlock(&store)?;
    let from: Address = store
        .address()
        .parse()
        .map_err(|_| "the wallet holds an address that cannot be parsed".to_owned())?;

    // 3. Nonce, gas and balance, fetched now rather than at T-0.
    let (nonce, gas_price, balance) = chain_state(&mut rpc, from).await?;

    // There is no priority auction on this chain: the sequencer is first come,
    // first served, so a tip buys nothing and would only be a donation. The
    // ceiling is doubled because the base fee can move between here and the
    // open, and the difference is refunded when it does not.
    let max_fee = gas_price.saturating_mul(2);
    let worst_case = u128::from(GAS_LIMIT).saturating_mul(max_fee);

    let tx = Eip1559 {
        chain_id: config.chain_id,
        nonce,
        max_priority_fee_per_gas: 0,
        max_fee_per_gas: max_fee,
        gas_limit: GAS_LIMIT,
        to: SEADROP
            .parse()
            .map_err(|_| "bad SeaDrop address".to_owned())?,
        value: 0,
        data: mint_public_calldata(collection, fee, args.quantity),
    };

    // 4. Signed before the wait, not after. At T-0 there must be nothing left to
    //    compute.
    let signed = tx
        .sign(&secret)
        .map_err(|e| format!("could not sign: {e}"))?;

    let mut clock = Clock::new();
    clock.sync().await;

    report(
        &drop,
        collection,
        fee,
        from,
        balance,
        worst_case,
        &signed,
        &clock,
        args.quantity,
    );

    let funded = balance >= worst_case;

    if !args.fire {
        if !funded {
            println!(
                "  NOT FUNDED. This holds {} ETH and the mint could cost up to {} ETH in gas.",
                format_eth(balance),
                format_eth(worst_case)
            );
            println!("  Send some to {from} before firing.\n");
        }
        println!(
            "  Nothing was sent. Add --fire to actually mint.\n\
             \n  Run this again close to the open: the nonce and gas price above were read\n  \
             now, and a stage can be reconfigured until the moment it starts.\n"
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Only a hard refusal when something is actually about to be spent. A dry
    // run should say everything it found, not stop at the first problem.
    if !funded {
        return Err(format!(
            "Refusing to fire. This wallet holds {} ETH and the mint could cost up to {} ETH \
             in gas. Send some to {from} first.",
            format_eth(balance),
            format_eth(worst_case)
        ));
    }

    fire(config, &clock, &drop, &signed, nonce, from, rpc).await
}

/// Waits for the open, sends, and reports what happened. Split from preparation
/// because this half is the only part that can cost anything, and the boundary is
/// where the decision to spend money actually sits.
async fn fire(
    config: &Config,
    clock: &Clock,
    drop: &PublicDrop,
    signed: &Signed,
    nonce: u64,
    from: Address,
    rpc: Rpc,
) -> Result<ExitCode, String> {
    clock
        .assert_usable()
        .map_err(|e| format!("refusing to fire: {e}"))?;

    // Wait, then one write per endpoint.
    let open_at_ms = i64::try_from(drop.start_time).unwrap_or(0) * 1_000;
    let remaining = open_at_ms - clock.now_ms();
    if remaining > READY_BY_SECONDS * 1_000 {
        println!(
            "  Waiting {} seconds for the stage to open.\n",
            remaining / 1_000
        );
    } else if remaining > 0 {
        println!("  Opens in {remaining} ms.\n");
    }
    clock.sleep_until(open_at_ms).await;

    // Drift can appear during the wait, so this is checked again rather than
    // trusted from before.
    clock
        .assert_usable()
        .map_err(|e| format!("refusing to fire, the clock moved during the wait: {e}"))?;

    let sent = send_everywhere(config, signed).await;
    println!("  {}", sent.summary);
    if !sent.accepted {
        return Err(format!(
            "no endpoint accepted the transaction. {}",
            sent.summary
        ));
    }

    // 6. What actually happened. Four states, and only one is a win.
    // Everything below reports what happened; none of it decides it.
    let mut probe = RpcProbe {
        rpc,
        hash: format!("{:?}", signed.hash),
        from,
        signed: signed.clone(),
        config,
    };
    let outcome = classify(&mut probe, nonce, ConfirmSettings::default()).await;
    println!("\n  {}", describe(&outcome, &format!("{:?}", signed.hash)));
    println!();

    Ok(if outcome.is_win() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Nonce, gas price and balance in one place, read during preparation so that
/// nothing at T-0 is waiting on a round trip.
async fn chain_state(rpc: &mut Rpc, from: Address) -> Result<(u64, u128, u128), String> {
    let nonce = parse_hex_u64(
        &rpc.call::<String>(
            "eth_getTransactionCount",
            json!([from.to_string(), "pending"]),
        )
        .await
        .map_err(|e| format!("could not read the nonce: {e}"))?,
    )
    .map_err(|e| e.to_string())?;

    let gas_price = parse_hex_u128(
        &rpc.call::<String>("eth_gasPrice", json!([]))
            .await
            .map_err(|e| format!("could not read the gas price: {e}"))?,
    )
    .map_err(|e| e.to_string())?;

    let balance = parse_hex_u128(
        &rpc.call::<String>("eth_getBalance", json!([from.to_string(), "latest"]))
            .await
            .map_err(|e| format!("could not read the balance: {e}"))?,
    )
    .map_err(|e| e.to_string())?;

    Ok((nonce, gas_price, balance))
}

fn unlock(store: &Keystore) -> Result<Zeroizing<[u8; 32]>, String> {
    use std::io::{BufRead, IsTerminal};
    let passphrase = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Passphrase for {}: ", store.address()))
            .map(Zeroizing::new)
            .map_err(|_| "could not read the passphrase".to_owned())?
    } else {
        let mut line = Zeroizing::new(String::new());
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|_| "could not read the passphrase".to_owned())?;
        Zeroizing::new(line.trim_end().to_owned())
    };
    store.decrypt(&passphrase).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn report(
    drop: &PublicDrop,
    collection: Address,
    fee: Address,
    from: Address,
    balance: u128,
    worst_case: u128,
    signed: &Signed,
    clock: &Clock,
    quantity: u64,
) {
    let opens = drop.start_time;
    println!("\n  collection   {collection}");
    println!(
        "  stage        opens at unix {opens}, {} per wallet",
        drop.max_per_wallet
    );
    println!("  price        free");
    println!("  fee to       {fee}");
    println!("  minting to   {from}   <- your wallet is the minter");
    println!("  quantity     {quantity}");
    println!("  balance      {} ETH", format_eth(balance));
    println!("  gas ceiling  {} ETH", format_eth(worst_case));
    println!("  tx hash      {:?}", signed.hash);
    println!("  calldata     {} bytes", signed.raw.len());
    match clock.assert_usable() {
        Ok(()) => println!("  clock        {} ms drift\n", clock.drift_ms()),
        Err(err) => println!("  clock        {err}\n"),
    }
}

struct Sent {
    accepted: bool,
    summary: String,
}

/// Pushes the same signed bytes to the sequencer and every read endpoint at
/// once. The sequencer is the only one that orders anything; the others are
/// additional ways in, not additional chances.
async fn send_everywhere(config: &Config, signed: &Signed) -> Sent {
    let raw = signed.raw_hex();
    let mut accepted = false;
    let mut notes = Vec::new();

    for url in config.send_urls() {
        let mut endpoint = Rpc::new(vec![url.clone()], Duration::from_secs(8));
        match endpoint
            .call::<String>("eth_sendRawTransaction", json!([raw]))
            .await
        {
            Ok(returned) => {
                // An endpoint that takes our bytes and answers with a different
                // hash has not sent our transaction, and believing it would mean
                // waiting for a receipt that can never arrive.
                if returned.eq_ignore_ascii_case(&format!("{:?}", signed.hash)) {
                    accepted = true;
                    notes.push(format!("{}: accepted", endpoint.endpoint()));
                } else {
                    notes.push(format!(
                        "{}: answered a different hash, {returned}",
                        endpoint.endpoint()
                    ));
                }
            }
            Err(RpcError::Rejected { message, .. }) => {
                notes.push(format!("{}: {message}", endpoint.endpoint()));
            }
            Err(err) => notes.push(format!("{}: {err}", endpoint.endpoint())),
        }
    }
    Sent {
        accepted,
        summary: notes.join("\n  "),
    }
}

struct RpcProbe<'a> {
    rpc: Rpc,
    hash: String,
    from: Address,
    signed: Signed,
    config: &'a Config,
}

impl ChainProbe for RpcProbe<'_> {
    async fn receipt(&mut self) -> Result<Option<String>, ()> {
        let value: Option<serde_json::Value> = self
            .rpc
            .call("eth_getTransactionReceipt", json!([self.hash]))
            .await
            .map_err(|_| ())?;
        Ok(value.and_then(|v| v.get("status").and_then(|s| s.as_str().map(str::to_owned))))
    }

    async fn seen(&mut self) -> Result<bool, ()> {
        let value: Option<serde_json::Value> = self
            .rpc
            .call("eth_getTransactionByHash", json!([self.hash]))
            .await
            .map_err(|_| ())?;
        Ok(value.is_some())
    }

    async fn nonce(&mut self) -> Result<u64, ()> {
        let hex: String = self
            .rpc
            .call(
                "eth_getTransactionCount",
                json!([self.from.to_string(), "pending"]),
            )
            .await
            .map_err(|_| ())?;
        parse_hex_u64(&hex).map_err(|_| ())
    }

    async fn resend(&mut self) -> Result<(), ()> {
        // The second ingress: the read endpoints reach the same sequencer by a
        // different network path, which is the only thing that could differ.
        let mut endpoint = Rpc::new(self.config.rpc_urls.clone(), Duration::from_secs(5));
        let _ = endpoint
            .call::<String>("eth_sendRawTransaction", json!([self.signed.raw_hex()]))
            .await;
        Ok(())
    }
}

fn describe(outcome: &Outcome, hash: &str) -> String {
    match outcome {
        Outcome::Included { reverted: false } => format!("MINTED. {hash}"),
        Outcome::Included { reverted: true } => {
            format!("It landed and reverted on chain, so nothing was minted. {hash}")
        }
        Outcome::Rejected { reason } => format!("Rejected. {reason}"),
        Outcome::Vanished { reason } => format!("VANISHED. {reason}. {hash}"),
        // Never described as a win. A dispatch is not an outcome.
        Outcome::Dispatched { reason } => {
            format!("No receipt yet, so this is not a win. {reason} {hash}")
        }
    }
}
