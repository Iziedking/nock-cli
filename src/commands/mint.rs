use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy_primitives::Address;
use serde_json::json;
use zeroize::Zeroizing;

use crate::chain::opensea::gql::{
    self, mint_action_variables, StageType, COLLECTION_METADATA, COLLECTION_SEARCH,
    DROP_ELIGIBILITY, MINT_ACTION,
};
use crate::chain::opensea::siwe::{authenticate, Session};
use crate::chain::opensea::verify::{verify, Expectation, Rejection};
use crate::chain::rpc::{parse_hex_u128, parse_hex_u64, Rpc, RpcError};
use crate::chain::seadrop::{
    fee_recipient, mint_public_calldata, public_drop, supply_left, PublicDrop, SEADROP,
};
use crate::chain::tx::{Eip1559, Signed};
use crate::commands::doctor::format_eth;
use crate::commands::report::{exit_code, render_outcome_table, render_plan_table, WalletOutcome};
use crate::config::Config;
use crate::engine::clock::Clock;
use crate::engine::confirm::{classify, ChainProbe, ConfirmSettings, Outcome};
use crate::engine::fire::{fire_all_with, Shot};
use crate::plan::planner::{build_plan, Candidate, StagePlan};
use crate::plan::spend::SpendCeiling;
use crate::plan::stage::Stage;
use crate::wallet::set::{read_set_file, unlock, WalletEntry, WalletSet};

/// Minting, from a wallet set, on a public or a signed stage.
///
/// The orchestration and nothing else. Every decision it makes lives somewhere
/// tested: what a stage is and when it moved in `plan::stage`, who is in it in
/// `plan::planner`, whether calldata can be trusted in `chain::opensea::verify`,
/// how the batch goes out in `engine::fire`, and what to say about it in
/// `commands::report`.
///
/// WHERE OPENSEA IS AND IS NOT INVOLVED. A public stage is built entirely from
/// chain data: the price, the window and the fee recipient are all readable, and
/// the calldata is four words we assemble ourselves. A signed stage cannot be,
/// because it needs a signature only `OpenSea` holds. So the third party sits on
/// the money path exactly where it is unavoidable and nowhere else.
const GAS_LIMIT: u64 = 320_000;

/// Preparation is done by here. After this the loop only waits and writes.
const FREEZE_SECONDS: i64 = 30;

/// Far enough out that waiting is worth saying out loud.
const READY_BY_SECONDS: i64 = 30;

pub struct MintArgs<'a> {
    pub collection: &'a str,
    pub quantity: u64,
    /// One wallet or a whole set. A single `--wallet` becomes a one-entry set so
    /// there is only one path below this point.
    pub wallets: Vec<PathBuf>,
    /// Without this nothing is sent. A command that spends money should not do
    /// so because somebody pressed up-arrow and enter.
    pub fire: bool,
    /// The most this whole run may spend on mint prices, in wei. Required once
    /// any stage costs anything.
    pub max_spend_wei: Option<u128>,
    /// Which stage to enter. Without it the run takes the earliest one that has
    /// not ended.
    pub stage: Option<u64>,
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
            "{} is not an address. It should be 0x and 40 hex characters.",
            args.collection
        )
    })?;

    let wallets = unlock_all(&args.wallets)?;
    println!("\n  {} wallet(s) unlocked", wallets.entries.len());

    let mut rpc = Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10));
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 nock")
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    let (stage, slug) = choose_stage(&mut rpc, &http, collection, args.stage).await?;
    println!(
        "  stage {} is {}, opening at unix {}",
        stage.index,
        if stage.is_signed() {
            "signed"
        } else {
            "public"
        },
        stage.start_time
    );

    if stage.price_wei > 0 && args.max_spend_wei.is_none() {
        return Err(format!(
            "This stage costs {} ETH each. Re-run with --max-spend to say what this run may spend.",
            format_eth(stage.price_wei)
        ));
    }
    let mut ceiling = SpendCeiling::new(args.max_spend_wei.unwrap_or(0));

    let fee = fee_recipient(&mut rpc, collection)
        .await
        .map_err(|e| format!("could not read the fee recipient: {e}"))?;

    // Everything each wallet needs, gathered before anything is signed.
    let mut prepared = Vec::with_capacity(wallets.entries.len());
    for entry in &wallets.entries {
        prepared.push(
            prepare_wallet(
                &mut rpc,
                &http,
                PrepareInput {
                    entry,
                    collection,
                    fee,
                    stage,
                    slug: slug.as_deref(),
                    quantity: args.quantity,
                },
            )
            .await,
        );
    }

    let candidates: Vec<Candidate> = prepared.iter().map(|p| p.candidate.clone()).collect();
    let plan = build_plan(stage, &candidates, &mut ceiling);
    println!("{}", render_plan_table(&plan, &ceiling));

    if !args.fire {
        println!("  Nothing was sent. Re-run with --fire when you mean it.\n");
        return Ok(ExitCode::SUCCESS);
    }
    if plan.ready().count() == 0 {
        return Err("no wallet is ready for this stage, so there is nothing to send.".to_owned());
    }

    fire_stage(config, rpc, &plan, &prepared).await
}

struct Prepared {
    candidate: Candidate,
    /// What this wallet would send, if it sends anything.
    calldata: Vec<u8>,
    value_wei: u128,
    nonce: u64,
    max_fee: u128,
    secret: Zeroizing<[u8; 32]>,
    address: Address,
}

struct PrepareInput<'a> {
    entry: &'a WalletEntry,
    collection: Address,
    fee: Address,
    stage: Stage,
    slug: Option<&'a str>,
    quantity: u64,
}

/// One wallet's nonce, balance, calldata and verdict.
///
/// Never returns an error: a wallet that cannot take part comes back with a
/// reason on its candidate, because the report promises a line for everybody.
async fn prepare_wallet(
    rpc: &mut Rpc,
    http: &reqwest::Client,
    input: PrepareInput<'_>,
) -> Prepared {
    let address = input.entry.address;
    let (nonce, gas_price, balance) = chain_state(rpc, address).await.unwrap_or((0, 0, 0));
    let max_fee = gas_price.saturating_mul(2);
    let gas_ceiling_wei = u128::from(GAS_LIMIT).saturating_mul(max_fee);
    let quantity = input.quantity.min(input.stage.max_per_wallet.max(1));

    let mut candidate = Candidate {
        index: input.entry.index,
        address,
        eligible: true,
        quantity,
        refusal: None,
        balance_wei: balance,
        gas_ceiling_wei,
        supply_left: supply_left(rpc, input.collection).await,
    };

    let (calldata, value_wei) = if input.stage.is_signed() {
        match signed_calldata(http, &input, address, quantity).await {
            Ok((data, value, refusal)) => {
                candidate.refusal = refusal;
                (data, value)
            }
            Err(why) => {
                // Not eligible is the ordinary answer here, so it is reported as
                // that rather than as a failure of ours.
                candidate.eligible = false;
                println!("  wallet {}: {why}", input.entry.index);
                (Vec::new(), 0)
            }
        }
    } else {
        // Public stages need nothing from anybody: four words, from chain data.
        (
            mint_public_calldata(input.collection, input.fee, quantity),
            input.stage.price_wei.saturating_mul(u128::from(quantity)),
        )
    };

    if calldata.is_empty() {
        candidate.eligible = false;
    }

    Prepared {
        candidate,
        calldata,
        value_wei,
        nonce,
        max_fee,
        secret: input.entry.secret.clone(),
        address,
    }
}

/// Calldata for a signed stage, which is the only thing `OpenSea` is asked for.
///
/// Returns the calldata, its value, and a refusal if verification found one. A
/// refusal is not an error: the wallet stays in the report with the field named.
async fn signed_calldata(
    http: &reqwest::Client,
    input: &PrepareInput<'_>,
    address: Address,
    quantity: u64,
) -> Result<(Vec<u8>, u128, Option<Rejection>), String> {
    let slug = input.slug.ok_or_else(|| {
        "this collection is not on OpenSea, so a signed stage cannot be minted here".to_owned()
    })?;

    let session: Session = authenticate(http, address, &input.entry.secret, 4663)
        .await
        .map_err(|e| format!("could not sign in to OpenSea: {e}"))?;

    let body = gql::post(
        http,
        DROP_ELIGIBILITY,
        json!({ "collectionSlug": slug, "address": format!("{address:?}") }),
        Some(&session),
    )
    .await
    .map_err(|e| format!("could not read eligibility: {e}"))?;

    let eligibility =
        gql::parse_eligibility(&body).map_err(|e| format!("could not read eligibility: {e}"))?;
    let mine = eligibility
        .iter()
        .find(|e| e.stage_index == input.stage.index)
        .ok_or_else(|| {
            format!(
                "stage {} was not in the eligibility answer",
                input.stage.index
            )
        })?;
    if !mine.is_eligible {
        return Err(format!("not on the list for stage {}", input.stage.index));
    }

    let body = gql::post(
        http,
        MINT_ACTION,
        mint_action_variables(address, input.collection, "robinhood", quantity),
        Some(&session),
    )
    .await
    .map_err(|e| format!("could not fetch calldata: {e}"))?;
    let submission =
        gql::parse_submission(&body).map_err(|e| format!("could not fetch calldata: {e}"))?;

    // Nothing reaches the signer until this passes.
    let expectation = Expectation {
        collection: input.collection,
        minter: address,
        quantity,
        unit_price_wei: input.stage.price_wei,
        allowed_fee_recipients: vec![input.fee],
        bounds: None,
        spend_remaining_wei: u128::MAX,
        stage_is_signed: true,
    };
    match verify(&submission, &expectation) {
        Ok(_) => Ok((submission.data, submission.value_wei, None)),
        Err(refusal) => Ok((Vec::new(), 0, Some(refusal))),
    }
}

/// Waits for the open and sends every ready wallet at once.
async fn fire_stage(
    config: &Config,
    rpc: Rpc,
    plan: &StagePlan,
    prepared: &[Prepared],
) -> Result<ExitCode, String> {
    let clock = Clock::new();
    let open_at_ms = i64::try_from(plan.stage.start_time).unwrap_or(0) * 1_000;
    let remaining = open_at_ms - clock.now_ms();

    if remaining > 0 {
        clock
            .assert_usable()
            .map_err(|e| format!("refusing to fire at a stage that has not opened: {e}"))?;
        if remaining > READY_BY_SECONDS * 1_000 {
            println!(
                "  Waiting {} seconds for the stage to open.",
                remaining / 1_000
            );
        } else {
            println!("  Opens in {remaining} ms.");
        }
    }

    // Signed before the wait, so at T-0 there is nothing left to compute.
    let mut shots = Vec::new();
    for prep in prepared {
        if !plan
            .wallets
            .iter()
            .any(|w| w.index == prep.candidate.index && w.status.is_ready())
        {
            continue;
        }
        let tx = Eip1559 {
            chain_id: config.chain_id,
            nonce: prep.nonce,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: prep.max_fee,
            gas_limit: GAS_LIMIT,
            to: SEADROP
                .parse()
                .map_err(|_| "bad SeaDrop address".to_owned())?,
            value: prep.value_wei,
            data: prep.calldata.clone(),
        };
        let signed = tx
            .sign(&prep.secret)
            .map_err(|e| format!("could not sign for {}: {e}", prep.address))?;
        shots.push(Shot {
            index: prep.candidate.index,
            address: prep.address,
            nonce: prep.nonce,
            signed,
        });
    }
    println!("  {} transaction(s) signed and frozen\n", shots.len());

    if remaining > 0 && remaining < FREEZE_SECONDS * 1_000 {
        println!("  Inside the freeze window. Nothing further will be fetched or re-signed.");
    }
    clock.sleep_until(open_at_ms).await;
    if remaining > 0 {
        clock
            .assert_usable()
            .map_err(|e| format!("refusing to fire, the clock moved during the wait: {e}"))?;
    }

    // The closure owns its endpoints rather than borrowing the config, because
    // each send runs on its own task and a borrow cannot outlive this function.
    let send_urls: Vec<String> = config.send_urls();
    let sent = fire_all_with(shots.clone(), move |shot: Shot| {
        let send_urls = send_urls.clone();
        async move {
            let out = send_to(&send_urls, &shot.signed).await;
            if out.accepted {
                Ok(format!("{:?}", shot.signed.hash))
            } else {
                Err(out.summary)
            }
        }
    })
    .await;

    // What actually happened, per wallet, from the chain rather than from the
    // endpoint that took the bytes.
    let mut results = Vec::with_capacity(sent.len());
    for (result, shot) in sent.iter().zip(shots.iter()) {
        let outcome = match &result.dispatch {
            Err(reason) => Outcome::Rejected {
                reason: reason.clone(),
            },
            Ok(hash) => {
                let mut probe = RpcProbe {
                    rpc: Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10)),
                    hash: hash.clone(),
                    from: shot.address,
                    signed: shot.signed.clone(),
                    config,
                };
                classify(&mut probe, shot.nonce, ConfirmSettings::default()).await
            }
        };
        results.push(WalletOutcome {
            index: result.index,
            address: result.address,
            outcome,
            tx_hash: result.dispatch.as_ref().ok().cloned(),
        });
    }

    drop(rpc);
    println!("{}", render_outcome_table(&results));
    Ok(exit_code(&results))
}

/// The stage to enter, and the `OpenSea` slug if the collection has one.
///
/// The chain is asked first and is enough on its own for a public stage. `OpenSea`
/// is consulted for the stage list because signed stages are invisible from
/// chain alone, and its absence is not fatal.
async fn choose_stage(
    rpc: &mut Rpc,
    http: &reqwest::Client,
    collection: Address,
    wanted: Option<u64>,
) -> Result<(Stage, Option<String>), String> {
    let on_chain: Option<PublicDrop> = public_drop(rpc, collection).await.ok();

    let mut slug = None;
    let mut stages: Vec<Stage> = Vec::new();
    if let Ok(body) = gql::post(
        http,
        COLLECTION_SEARCH,
        json!({ "query": format!("{collection:?}") }),
        None,
    )
    .await
    {
        if let Ok(found) = gql::parse_collection(&body, collection) {
            if let Ok(meta) = gql::post(
                http,
                COLLECTION_METADATA,
                json!({ "slug": found.slug }),
                None,
            )
            .await
            {
                if let Ok(list) = gql::parse_metadata(&meta) {
                    stages = list
                        .iter()
                        .map(|m| {
                            let price = if m.stage_type == StageType::PublicSale {
                                on_chain.map_or(0, |d| d.mint_price_wei)
                            } else {
                                // A signed stage's price is only knowable from the
                                // calldata, which is verified before it is used.
                                0
                            };
                            Stage::from_meta(m, price)
                        })
                        .collect();
                }
            }
            slug = Some(found.slug);
        }
    }

    if stages.is_empty() {
        let drop = on_chain.ok_or_else(|| {
            "no public stage on chain and nothing on OpenSea, so there is nothing to mint"
                .to_owned()
        })?;
        stages.push(Stage {
            index: 0,
            kind: StageType::PublicSale,
            start_time: drop.start_time,
            end_time: drop.end_time,
            price_wei: drop.mint_price_wei,
            max_per_wallet: u64::from(drop.max_per_wallet),
        });
    }

    let now = now_unix();
    // Merkle allowlists are not served: mintAllowList takes a proof this tool
    // does not build, and letting one through would build public calldata for an
    // allowlist stage.
    let (mintable, refused): (Vec<Stage>, Vec<Stage>) =
        stages.into_iter().partition(Stage::is_mintable);
    for stage in &refused {
        println!(
            "  stage {} is a merkle allowlist, which this tool does not mint",
            stage.index
        );
    }
    let stages = mintable;

    // The earliest stage that has not already ended, so a run started early
    // walks into the first thing it can actually mint.
    let chosen = if let Some(index) = wanted {
        stages
            .into_iter()
            .find(|s| s.index == index)
            .ok_or_else(|| format!("this drop has no stage {index}"))?
    } else {
        {
            let mut live: Vec<Stage> = stages.into_iter().filter(|s| s.end_time > now).collect();
            live.sort_by_key(|s| s.start_time);
            live.into_iter()
                .next()
                .ok_or_else(|| "every stage on this drop has ended".to_owned())?
        }
    };
    Ok((chosen, slug))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn unlock_all(paths: &[PathBuf]) -> Result<WalletSet, String> {
    if paths.is_empty() {
        return Err("no wallet was given. Use --wallet or --wallet-set.".to_owned());
    }
    let passphrase = read_passphrase(paths.len())?;
    unlock(paths, &passphrase).map_err(|e| e.to_string())
}

/// One prompt for the whole set. Asking once per wallet under time pressure is
/// how people end up leaving keys unlocked somewhere convenient.
fn read_passphrase(count: usize) -> Result<Zeroizing<String>, String> {
    use std::io::{BufRead, IsTerminal};
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("Passphrase for {count} wallet(s): "))
            .map(Zeroizing::new)
            .map_err(|_| "could not read the passphrase".to_owned())
    } else {
        let mut line = Zeroizing::new(String::new());
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|_| "could not read the passphrase".to_owned())?;
        Ok(Zeroizing::new(line.trim_end().to_owned()))
    }
}

/// Reads the set file, or treats a single path as a set of one.
pub fn wallet_paths(
    single: Option<&PathBuf>,
    set: Option<&PathBuf>,
) -> Result<Vec<PathBuf>, String> {
    match (single, set) {
        (_, Some(file)) => {
            let text = std::fs::read_to_string(file)
                .map_err(|e| format!("could not read {}: {e}", file.display()))?;
            let base = file.parent().unwrap_or_else(|| std::path::Path::new("."));
            read_set_file(&text, base).map_err(|e| e.to_string())
        }
        (Some(one), None) => Ok(vec![one.clone()]),
        (None, None) => Err("no wallet was given. Use --wallet or --wallet-set.".to_owned()),
    }
}

async fn chain_state(rpc: &mut Rpc, from: Address) -> Result<(u64, u128, u128), String> {
    let nonce: String = rpc
        .call(
            "eth_getTransactionCount",
            json!([format!("{from:?}"), "pending"]),
        )
        .await
        .map_err(|e| format!("could not read the nonce: {e}"))?;
    let gas_price: String = rpc
        .call("eth_gasPrice", json!([]))
        .await
        .map_err(|e| format!("could not read the gas price: {e}"))?;
    let balance: String = rpc
        .call("eth_getBalance", json!([format!("{from:?}"), "latest"]))
        .await
        .map_err(|e| format!("could not read the balance: {e}"))?;

    Ok((
        parse_hex_u64(&nonce).map_err(|e| e.to_string())?,
        parse_hex_u128(&gas_price).map_err(|e| e.to_string())?,
        parse_hex_u128(&balance).map_err(|e| e.to_string())?,
    ))
}

struct Sent {
    accepted: bool,
    summary: String,
}

async fn send_everywhere(config: &Config, signed: &Signed) -> Sent {
    send_to(&config.send_urls(), signed).await
}

async fn send_to(urls: &[String], signed: &Signed) -> Sent {
    let raw = signed.raw_hex();
    let mut accepted = false;
    let mut notes = Vec::new();

    for url in urls {
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
        summary: notes.join("; "),
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
        Ok(value.and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_owned)))
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
        let raw: String = self
            .rpc
            .call(
                "eth_getTransactionCount",
                json!([format!("{:?}", self.from), "latest"]),
            )
            .await
            .map_err(|_| ())?;
        parse_hex_u64(&raw).map_err(|_| ())
    }

    async fn resend(&mut self) -> Result<(), ()> {
        let out = send_everywhere(self.config, &self.signed).await;
        if out.accepted {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single --wallet is a set of one, so nothing below the unlock has two
    // code paths to keep in step.
    #[test]
    fn a_single_wallet_is_a_set_of_one() {
        let one = PathBuf::from("a.json");
        assert_eq!(wallet_paths(Some(&one), None).unwrap(), vec![one]);
    }

    #[test]
    fn it_refuses_a_run_with_no_wallet_at_all() {
        assert!(wallet_paths(None, None).is_err());
    }

    #[test]
    fn it_reports_a_set_file_it_cannot_read_rather_than_running_empty() {
        let missing = PathBuf::from("does-not-exist.txt");
        let err = wallet_paths(None, Some(&missing)).unwrap_err();
        assert!(err.contains("could not read"));
    }
}
