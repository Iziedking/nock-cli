use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use alloy_primitives::Address;
use serde_json::json;
use zeroize::Zeroizing;

use crate::chain::opensea::gql::{
    self, mint_action_variables, slug_from_link, StageType, COLLECTION_METADATA, COLLECTION_SEARCH,
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
/// A bounded signed gas ceiling. Unused gas is not charged, but the estimate
/// must fit with 20% headroom before any transaction is broadcast.
const GAS_LIMIT: u64 = 1_000_000;

/// Preparation is done by here. After this the loop only waits and writes.
const FREEZE_SECONDS: i64 = 30;

/// Far enough out that waiting is worth saying out loud.
const READY_BY_SECONDS: i64 = 30;

/// `OpenSea`'s eligibility view can briefly lag the mint action used by its UI.
/// Keep the fallback short: it is for a settling response, not for waiting
/// through a closed or sold-out stage.
const MINT_ACTION_RETRIES: usize = 3;
const MINT_ACTION_RETRY_DELAY: Duration = Duration::from_secs(2);
const MINT_ACTION_RATE_LIMIT_DELAY: Duration = Duration::from_secs(5);
const MINT_ACTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Do not spend the whole final minute polling `OpenSea`. Its signed action is
/// useful only near opening, and a warm request at T-5 leaves enough time to
/// absorb cache lag without needlessly consuming the endpoint's rate limit.
const MINT_ACTION_FIRST_CHECK_SECONDS: u64 = 5;
/// A signed mint action can appear only after the stage opens, even when
/// `OpenSea` published the stage hours earlier. An armed fire run waits through
/// that cache-settling boundary instead of spending its only attempt early.
const MINT_ACTION_OPEN_GRACE_SECONDS: u64 = 30;
/// A bad timestamp must not leave a command waiting without a useful bound.
const MINT_ACTION_MAX_WAIT_SECONDS: u64 = 120;
const PREFLIGHT_RETRIES: usize = 3;
const PREFLIGHT_RETRY_DELAY: Duration = Duration::from_secs(1);
const PREFLIGHT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PREFLIGHT_OPEN_GRACE_SECONDS: u64 = 30;
/// A stalled primary send must not hold the Alchemy and public fallbacks for
/// eight seconds each. Re-sending the same signed bytes cannot mint twice.
const SEND_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryPlan {
    initial_delay: Duration,
    deadline_after: Option<Duration>,
    max_attempts: Option<usize>,
}

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
    /// Cron-only durable marker. If it cannot be written, sending is forbidden.
    pub broadcast_intent_file: Option<PathBuf>,
}

pub struct EligibilityArgs<'a> {
    pub collection: &'a str,
    pub wallets: Vec<PathBuf>,
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

/// Read the wallet-specific `OpenSea` answer for every phase without requesting
/// mint calldata. This is deliberately separate from `mint`: eligibility is a
/// useful preflight result even while a stage is still closed.
pub async fn eligibility(config: &Config, args: EligibilityArgs<'_>) -> ExitCode {
    match eligibility_report(config, args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("\n  {message}\n");
            ExitCode::FAILURE
        }
    }
}

async fn eligibility_report(_config: &Config, args: EligibilityArgs<'_>) -> Result<(), String> {
    let wallets = unlock_all(&args.wallets)?;
    println!("\n  {} wallet(s) unlocked", wallets.entries.len());

    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 nock")
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;
    let (collection, slug) = resolve_collection(&http, args.collection).await?;
    let slug = slug.ok_or_else(|| {
        "eligibility needs an OpenSea collection link or slug, not only a contract address"
            .to_owned()
    })?;
    println!("  {slug} resolves to {collection:?}");

    for entry in &wallets.entries {
        let stages = read_eligibility(&http, &slug, entry.address, &entry.secret)
            .await
            .map_err(|e| format!("wallet {}: {e}", entry.index))?;
        println!("  wallet {} ({:?})", entry.index, entry.address);
        for stage in stages {
            println!(
                "    stage {} ({}) eligible={} cap={} price={}",
                stage.stage_index,
                stage_type_name(stage.stage_type),
                stage.is_eligible,
                stage
                    .max_total_mintable_by_wallet
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                stage.quoted_price.as_deref().unwrap_or("unknown")
            );
        }
    }
    println!("\n  Nothing was signed or sent.");
    Ok(())
}

fn stage_type_name(stage_type: StageType) -> &'static str {
    match stage_type {
        StageType::PublicSale => "public",
        StageType::SignedPresale => "signed",
        StageType::MerklePresale => "merkle",
    }
}

async fn prepare_and_run(config: &Config, args: MintArgs<'_>) -> Result<ExitCode, String> {
    let wallets = unlock_all(&args.wallets)?;
    println!("\n  {} wallet(s) unlocked", wallets.entries.len());

    let mut rpc = Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10));
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 nock")
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;

    // An address, an OpenSea link or a bare slug. Somebody looking at a drop
    // has the link; making them find forty hex characters they cannot
    // proofread is friction at exactly the wrong moment.
    let (collection, known_slug) = resolve_collection(&http, args.collection).await?;
    if let Some(slug) = &known_slug {
        println!("  {slug} resolves to {collection:?}");
    }

    let (stage, slug) = choose_stage(&mut rpc, &http, collection, args.stage, known_slug).await?;
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
                    fire: args.fire,
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

    fire_stage(
        config,
        rpc,
        &plan,
        &prepared,
        args.broadcast_intent_file.as_deref(),
    )
    .await
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
    fire: bool,
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
    let chain = chain_state(rpc, address).await;
    let (nonce, gas_price, balance) = chain.as_ref().copied().unwrap_or((0, 0, 0));
    let max_fee = gas_price.saturating_mul(2);
    let gas_ceiling_wei = u128::from(GAS_LIMIT).saturating_mul(max_fee);
    let quantity = input.quantity.min(input.stage.max_per_wallet.max(1));

    let mut candidate = Candidate {
        index: input.entry.index,
        address,
        eligible: true,
        quantity,
        unavailable: None,
        refusal: None,
        balance_wei: balance,
        gas_ceiling_wei,
        supply_left: supply_left(rpc, input.collection).await,
    };
    if let Err(error) = chain {
        candidate.unavailable = Some(format!("wallet chain state unavailable: {error}"));
    }

    let (calldata, value_wei) = if input.stage.is_signed() {
        match signed_calldata(http, &input, address, quantity).await {
            Ok((data, value, refusal)) => {
                candidate.refusal = refusal;
                (data, value)
            }
            Err(failure) => {
                // A verdict and a silence get different homes. Only the first
                // is allowed to say anything about this wallet's entitlement.
                match &failure {
                    CalldataFailure::Refused(_) => candidate.eligible = false,
                    CalldataFailure::Unavailable(why) => {
                        candidate.unavailable = Some(why.clone());
                    }
                }
                println!("  wallet {}: {}", input.entry.index, failure.why());
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

/// Why a signed stage produced no calldata, and whether that is a verdict.
///
/// THE WHOLE POINT IS THE DISTINCTION. `Refused` is `OpenSea` understanding the
/// question and answering that this mint cannot happen, which is final and is
/// about the wallet. `Unavailable` is the ABSENCE of an answer -- rate limited,
/// timed out, unreachable -- which says nothing whatever about the wallet.
///
/// Collapsing the two into one string is what reported a Goat Street wallet as
/// "not on the list" on 2026-08-28 after `OpenSea` rate limited the CLI. The
/// wallet was on the list and the web UI minted with it minutes later, so the
/// report sent its operator to check an allowlist spot that was never the
/// problem.
enum CalldataFailure {
    Refused(String),
    Unavailable(String),
}

impl CalldataFailure {
    fn why(&self) -> &str {
        match self {
            Self::Refused(why) | Self::Unavailable(why) => why,
        }
    }
}

/// True only when `OpenSea` understood the question and answered it.
///
/// Everything else -- transport, 429, a timeout, a changed schema -- is silence,
/// and silence must never be reported as ineligibility. Erring this way is the
/// safe direction: calling a real refusal "unknown" costs a line of report,
/// while calling silence a refusal costs the mint and blames the user.
fn is_an_answer(error: &gql::GqlError) -> bool {
    match error {
        gql::GqlError::Refused(reason) => !is_stage_not_open_reason(reason),
        _ => false,
    }
}

fn is_stage_not_open_reason(reason: &str) -> bool {
    let compact = reason
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    compact.contains("dropnotmintingerror")
        || compact.contains("mintstagenotopen")
        || compact.contains("mintnotstarted")
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
) -> Result<(Vec<u8>, u128, Option<Rejection>), CalldataFailure> {
    let slug = input.slug.ok_or_else(|| {
        CalldataFailure::Unavailable(
            "this collection is not on OpenSea, so a signed stage cannot be minted here".to_owned(),
        )
    })?;

    let (session, eligibility) = read_eligibility_session(http, slug, address, &input.entry.secret)
        .await
        .map_err(CalldataFailure::Unavailable)?;
    let mine = eligibility
        .iter()
        .find(|e| e.stage_index == input.stage.index)
        .ok_or_else(|| {
            CalldataFailure::Unavailable(format!(
                "stage {} was not in the eligibility answer",
                input.stage.index
            ))
        })?;
    if !mine.is_eligible {
        // This is not enough to stop. The web UI can show the same wallet as
        // ineligible for one request and eligible after a refresh while its
        // signed mint action is already available. The action response is the
        // stronger check because it contains the collection's authorization.
        println!(
            "  OpenSea eligibility is unsettled for stage {}; checking the live mint action",
            input.stage.index
        );
    }

    let submission = request_signed_mint_action(
        http,
        &session,
        address,
        input.collection,
        input.stage.index,
        quantity,
        mint_action_retry_plan(input.stage.start_time, input.fire, now_unix()),
    )
    .await?;

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

async fn read_eligibility(
    http: &reqwest::Client,
    slug: &str,
    address: Address,
    secret: &Zeroizing<[u8; 32]>,
) -> Result<Vec<gql::Eligibility>, String> {
    let (_, eligibility) = read_eligibility_session(http, slug, address, secret).await?;
    Ok(eligibility)
}

async fn read_eligibility_session(
    http: &reqwest::Client,
    slug: &str,
    address: Address,
    secret: &Zeroizing<[u8; 32]>,
) -> Result<(Session, Vec<gql::Eligibility>), String> {
    let session: Session = authenticate(http, address, secret, 4663)
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
    Ok((session, eligibility))
}

/// Ask `OpenSea` for the transaction it would actually give the web UI.
///
/// A short retry absorbs the observed eligibility-cache race. We still require
/// the normal calldata verification below, so a successful fallback can only
/// produce a transaction authorized for this wallet and signed stage.
async fn request_signed_mint_action(
    http: &reqwest::Client,
    session: &Session,
    address: Address,
    collection: Address,
    stage_index: u64,
    quantity: u64,
    retry: RetryPlan,
) -> Result<crate::chain::opensea::verify::SubmissionData, CalldataFailure> {
    let variables = mint_action_variables(address, collection, "robinhood", quantity);
    let mut last_error = None;
    let mut last_error_was_an_answer = false;
    let mut attempts = 0_usize;
    let deadline = retry.deadline_after.map(|after| Instant::now() + after);

    if !retry.initial_delay.is_zero() {
        tokio::time::sleep(retry.initial_delay).await;
    }

    loop {
        if retry.max_attempts.is_some_and(|limit| attempts >= limit) {
            break;
        }
        let request_timeout = deadline.map_or(MINT_ACTION_REQUEST_TIMEOUT, |until| {
            until
                .saturating_duration_since(Instant::now())
                .min(MINT_ACTION_REQUEST_TIMEOUT)
        });
        if request_timeout.is_zero() {
            break;
        }

        attempts = attempts.saturating_add(1);
        let result = match tokio::time::timeout(
            request_timeout,
            gql::post(http, MINT_ACTION, variables.clone(), Some(session)),
        )
        .await
        {
            Ok(Ok(body)) => gql::parse_submission(&body),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                last_error = Some(format!(
                    "OpenSea did not answer within {} seconds",
                    request_timeout.as_secs()
                ));
                if retry.max_attempts.is_some_and(|limit| attempts >= limit) {
                    break;
                }
                if !sleep_before_retry(MINT_ACTION_RETRY_DELAY, deadline).await {
                    break;
                }
                continue;
            }
        };
        match result {
            Ok(submission) => return Ok(submission),
            Err(gql::GqlError::Malformed(reason)) => {
                return Err(CalldataFailure::Unavailable(format!(
                    "could not read what OpenSea returned: {reason}"
                )));
            }
            Err(error) => {
                let delay = mint_action_retry_delay(&error);
                last_error = Some(error.to_string());
                last_error_was_an_answer = is_an_answer(&error);
                if mint_action_refusal_is_final(&error) {
                    break;
                }
                if retry.max_attempts.is_some_and(|limit| attempts >= limit) {
                    break;
                }
                if !sleep_before_retry(delay, deadline).await {
                    break;
                }
            }
        }
    }

    // The distinction the Goat Street failure turned on. A refusal is OpenSea
    // answering; anything else is OpenSea not answering, and only the first of
    // those says a single thing about whether this wallet is on the list.
    let why = last_error.unwrap_or_else(|| "no transaction was returned".to_owned());
    Err(if last_error_was_an_answer {
        CalldataFailure::Refused(format!("stage {stage_index}: {why}"))
    } else {
        CalldataFailure::Unavailable(format!(
            "OpenSea returned no mint action for stage {stage_index} after {attempts} attempt(s): {why}"
        ))
    })
}

async fn sleep_before_retry(delay: Duration, deadline: Option<Instant>) -> bool {
    let sleep_for = deadline.map_or(delay, |until| {
        until.saturating_duration_since(Instant::now()).min(delay)
    });
    if sleep_for.is_zero() {
        return false;
    }
    tokio::time::sleep(sleep_for).await;
    deadline.is_none_or(|until| Instant::now() < until)
}

fn mint_action_retry_delay(error: &gql::GqlError) -> Duration {
    match error {
        gql::GqlError::Status { status: 429 } => MINT_ACTION_RATE_LIMIT_DELAY,
        gql::GqlError::Query(message)
            if message.to_ascii_lowercase().contains("too many requests") =>
        {
            MINT_ACTION_RATE_LIMIT_DELAY
        }
        _ => MINT_ACTION_RETRY_DELAY,
    }
}

fn mint_action_refusal_is_final(error: &gql::GqlError) -> bool {
    matches!(
        error,
        gql::GqlError::Refused(reason) if reason.contains("InsufficientMintsRemainingError")
    )
}

fn mint_action_retry_plan(stage_start: u64, fire: bool, now: u64) -> RetryPlan {
    let deadline = stage_start.saturating_add(MINT_ACTION_OPEN_GRACE_SECONDS);
    if !fire || now >= deadline {
        return RetryPlan {
            initial_delay: Duration::ZERO,
            deadline_after: None,
            max_attempts: Some(MINT_ACTION_RETRIES),
        };
    }

    let budget_seconds = deadline
        .saturating_sub(now)
        .min(MINT_ACTION_MAX_WAIT_SECONDS);
    let first_check = stage_start.saturating_sub(MINT_ACTION_FIRST_CHECK_SECONDS);
    let initial_delay_seconds = first_check
        .saturating_sub(now)
        .min(budget_seconds.saturating_sub(1));

    RetryPlan {
        initial_delay: Duration::from_secs(initial_delay_seconds),
        deadline_after: Some(Duration::from_secs(budget_seconds)),
        max_attempts: None,
    }
}

/// Signs the ready wallets before the freeze window begins.
fn sign_ready_shots(
    config: &Config,
    plan: &StagePlan,
    prepared: &[Prepared],
) -> Result<Vec<Shot>, String> {
    let to = SEADROP
        .parse()
        .map_err(|_| "bad SeaDrop address".to_owned())?;
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
            to,
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
            value_wei: prep.value_wei,
            calldata: prep.calldata.clone(),
            signed,
        });
    }
    println!("  {} transaction(s) signed and frozen\n", shots.len());
    Ok(shots)
}

/// Simulates each exact transaction against the latest chain state.
async fn preflight_shots(rpc: &mut Rpc, shots: &[Shot]) -> Result<(), String> {
    for shot in shots {
        let tx = json!({
            "from": format!("{:?}", shot.address),
            "to": SEADROP,
            "value": format!("0x{:x}", shot.value_wei),
            "data": format!("0x{}", hex::encode(&shot.calldata)),
        });
        let estimate = rpc
            .call::<String>("eth_estimateGas", json!([tx, "latest"]))
            .await
            .map_err(|e| {
                format!(
                    "preflight refused wallet {} immediately before send: {e}",
                    shot.index
                )
            })?;
        validate_gas_estimate(&estimate)
            .map_err(|error| format!("preflight refused wallet {}: {error}", shot.index))?;
    }
    Ok(())
}

fn validate_gas_estimate(raw: &str) -> Result<u64, String> {
    let estimate =
        parse_hex_u64(raw).map_err(|error| format!("invalid gas estimate {raw}: {error}"))?;
    if estimate == 0 {
        return Err("zero gas estimate".to_owned());
    }
    let padded = estimate
        .checked_mul(6)
        .ok_or_else(|| "gas estimate overflow".to_owned())?
        .div_ceil(5);
    if padded > GAS_LIMIT {
        return Err(format!(
            "gas estimate {estimate} plus 20% headroom exceeds signed {GAS_LIMIT} gas limit"
        ));
    }
    Ok(estimate)
}

async fn preflight_shots_through_open(
    rpc: &mut Rpc,
    shots: &[Shot],
    stage_start: u64,
) -> Result<(), String> {
    let retry = preflight_retry_plan(stage_start, now_unix());
    let deadline = retry.deadline_after.map(|after| Instant::now() + after);
    let mut last_error = None;
    let mut attempts = 0_usize;

    loop {
        if retry.max_attempts.is_some_and(|limit| attempts >= limit) {
            break;
        }
        let request_timeout = deadline.map_or(PREFLIGHT_REQUEST_TIMEOUT, |until| {
            until
                .saturating_duration_since(Instant::now())
                .min(PREFLIGHT_REQUEST_TIMEOUT)
        });
        if request_timeout.is_zero() {
            break;
        }

        attempts = attempts.saturating_add(1);
        match tokio::time::timeout(request_timeout, preflight_shots(rpc, shots)).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(format!(
                    "mint preflight did not answer within {} seconds",
                    request_timeout.as_secs()
                ));
            }
        }
        if retry.max_attempts.is_some_and(|limit| attempts >= limit) {
            break;
        }
        if !sleep_before_retry(PREFLIGHT_RETRY_DELAY, deadline).await {
            break;
        }
    }
    Err(last_error
        .unwrap_or_else(|| format!("mint preflight did not succeed after {attempts} attempts")))
}

fn preflight_retry_plan(stage_start: u64, now: u64) -> RetryPlan {
    let deadline = stage_start.saturating_add(PREFLIGHT_OPEN_GRACE_SECONDS);
    if now >= deadline {
        return RetryPlan {
            initial_delay: Duration::ZERO,
            deadline_after: None,
            max_attempts: Some(PREFLIGHT_RETRIES),
        };
    }
    RetryPlan {
        initial_delay: Duration::ZERO,
        deadline_after: Some(Duration::from_secs(deadline.saturating_sub(now))),
        max_attempts: None,
    }
}

/// Sends the frozen transactions and classifies each result from chain state.
async fn send_and_classify(config: &Config, shots: &[Shot]) -> Vec<WalletOutcome> {
    // The closure owns its endpoints rather than borrowing the config, because
    // each send runs on its own task and a borrow cannot outlive this function.
    let send_urls: Vec<String> = config.send_urls();
    let sent = fire_all_with(shots.to_vec(), move |shot: Shot| {
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
    results
}

/// Waits for the open and sends every ready wallet at once.
async fn fire_stage(
    config: &Config,
    mut rpc: Rpc,
    plan: &StagePlan,
    prepared: &[Prepared],
    broadcast_intent_file: Option<&Path>,
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
    let shots = sign_ready_shots(config, plan, prepared)?;
    if remaining > 0 && remaining < FREEZE_SECONDS * 1_000 {
        println!("  Inside the freeze window. Nothing further will be fetched or re-signed.");
    }
    clock.sleep_until(open_at_ms).await;
    if remaining > 0 {
        clock
            .assert_usable()
            .map_err(|e| format!("refusing to fire, the clock moved during the wait: {e}"))?;
    }

    // On Robinhood Chain a reverting mint still costs gas. A single preflight
    // refusal aborts the batch rather than letting the other wallets race into
    // a state we have not proved safe.
    preflight_shots_through_open(&mut rpc, &shots, plan.stage.start_time).await?;
    if let Some(path) = broadcast_intent_file {
        persist_broadcast_intent(path)?;
    }
    let results = send_and_classify(config, &shots).await;

    drop(rpc);
    println!("{}", render_outcome_table(&results));
    Ok(exit_code(&results))
}

fn persist_broadcast_intent(path: &Path) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "cannot create broadcast-intent marker {}: {error}",
                path.display()
            )
        })?;
    file.write_all(b"broadcast-intent\n")
        .map_err(|error| format!("cannot write broadcast-intent marker: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync broadcast-intent marker: {error}"))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("cannot sync broadcast-intent directory: {error}"))?;
    }
    Ok(())
}

/// The stage to enter, and the `OpenSea` slug if the collection has one.
///
/// The chain is asked first and is enough on its own for a public stage. `OpenSea`
/// is consulted for the stage list because signed stages are invisible from
/// chain alone, and its absence is not fatal.
pub(crate) async fn choose_stage(
    rpc: &mut Rpc,
    http: &reqwest::Client,
    collection: Address,
    wanted: Option<u64>,
    known_slug: Option<String>,
) -> Result<(Stage, Option<String>), String> {
    let on_chain: Option<PublicDrop> = public_drop(rpc, collection).await.ok();

    let mut slug = known_slug;
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
                    if let Some(public) =
                        list.iter().find(|m| m.stage_type == StageType::PublicSale)
                    {
                        let drop = on_chain.ok_or_else(|| {
                            "OpenSea published a public stage, but the chain did not return a public drop".to_owned()
                        })?;
                        if public.start_time != drop.start_time
                            || public.end_time != drop.end_time
                            || public.max_total_mintable_by_wallet != u64::from(drop.max_per_wallet)
                        {
                            return Err(format!(
                                "OpenSea and Robinhood Chain disagree about the public stage: OpenSea {}-{} ({} per wallet), chain {}-{} ({} per wallet). Re-run after the collection updates its on-chain schedule.",
                                public.start_time,
                                public.end_time,
                                public.max_total_mintable_by_wallet,
                                drop.start_time,
                                drop.end_time,
                                drop.max_per_wallet
                            ));
                        }
                    }
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

/// Turns whatever the user typed into a contract address.
///
/// An address is taken as given. Anything else is treated as an `OpenSea` slug,
/// or a link with one in it, and resolved through them. Returning the slug too
/// saves resolving it a second time for the stage list.
pub(crate) async fn resolve_collection(
    http: &reqwest::Client,
    input: &str,
) -> Result<(Address, Option<String>), String> {
    let trimmed = input.trim();
    if let Ok(address) = trimmed.parse::<Address>() {
        return Ok((address, None));
    }

    let slug = slug_from_link(trimmed);
    if slug.is_empty() {
        return Err(format!("{input} is neither an address nor an OpenSea link"));
    }

    let body = gql::post(http, COLLECTION_METADATA, json!({ "slug": slug }), None)
        .await
        .map_err(|e| format!("could not look up {slug} on OpenSea: {e}"))?;
    let address = gql::parse_collection_address(&body).map_err(|e| {
        format!("{input} is not an address, and OpenSea has no collection called {slug}: {e}")
    })?;
    Ok((address, Some(slug.to_owned())))
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
        let mut endpoint = Rpc::new(vec![url.clone()], SEND_ENDPOINT_TIMEOUT);
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

    #[test]
    fn a_fire_run_uses_a_real_deadline_and_waits_until_near_opening() {
        let early = mint_action_retry_plan(1_000, true, 940);
        assert_eq!(early.initial_delay, Duration::from_secs(55));
        assert_eq!(early.deadline_after, Some(Duration::from_secs(90)));
        assert_eq!(early.max_attempts, None);

        let open = mint_action_retry_plan(1_000, true, 1_020);
        assert_eq!(open.initial_delay, Duration::ZERO);
        assert_eq!(open.deadline_after, Some(Duration::from_secs(10)));

        let late = mint_action_retry_plan(1_000, true, 1_031);
        assert_eq!(late.max_attempts, Some(3));
        assert_eq!(
            mint_action_retry_plan(1_000, false, 940).max_attempts,
            Some(3)
        );
    }

    #[test]
    fn preflight_absorbs_a_lagging_rpc_at_stage_open() {
        assert_eq!(
            preflight_retry_plan(1_000, 1_000).deadline_after,
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            preflight_retry_plan(1_000, 1_020).deadline_after,
            Some(Duration::from_secs(10))
        );
        assert_eq!(preflight_retry_plan(1_000, 1_031).max_attempts, Some(3));
    }

    #[test]
    fn gas_estimate_requires_headroom_inside_signed_limit() {
        assert_eq!(validate_gas_estimate("0xc3500"), Ok(800_000));
        assert_eq!(validate_gas_estimate("0xcb735"), Ok(833_333));
        assert!(validate_gas_estimate("0xcb736").is_err());
        assert!(validate_gas_estimate("0xd59f8").is_err());
        assert!(validate_gas_estimate("0x0").is_err());
        assert!(validate_gas_estimate("not-hex").is_err());
        assert!(validate_gas_estimate("0xffffffffffffffff").is_err());
    }

    #[test]
    fn durable_broadcast_intent_prevents_a_second_send() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nock-broadcast-intent-test-{}-{unique}",
            std::process::id()
        ));
        assert!(persist_broadcast_intent(&path).is_ok());
        assert!(persist_broadcast_intent(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

    // The Goat Street rule. Only a refusal is an answer; a 429, a dropped
    // connection or a timeout is silence, and silence may not be reported as
    // "not on the list" because that sends the user to check their own
    // allowlist spot for a fault that is ours.
    #[test]
    fn only_an_opensea_refusal_counts_as_an_answer() {
        assert!(is_an_answer(&gql::GqlError::Refused(
            "InsufficientMintsRemainingError".to_owned()
        )));
        assert!(!is_an_answer(&gql::GqlError::Status { status: 429 }));
        assert!(!is_an_answer(&gql::GqlError::Status { status: 503 }));
        assert!(!is_an_answer(&gql::GqlError::Transport("reset".to_owned())));
        assert!(!is_an_answer(&gql::GqlError::Query(
            "Too Many Requests".to_owned()
        )));
        assert!(!is_an_answer(&gql::GqlError::Missing("actions")));
    }

    #[test]
    fn a_closed_stage_is_not_reported_as_wallet_ineligibility() {
        assert!(!is_an_answer(&gql::GqlError::Refused(
            "DropNotMintingError".to_owned()
        )));
        assert!(!is_an_answer(&gql::GqlError::Refused(
            "MintStageNotOpen".to_owned()
        )));
        assert!(is_an_answer(&gql::GqlError::Refused(
            "MintWalletIneligible".to_owned()
        )));
    }

    #[test]
    fn a_calldata_failure_reports_its_reason_whichever_kind_it_is() {
        assert_eq!(CalldataFailure::Refused("no".to_owned()).why(), "no");
        assert_eq!(CalldataFailure::Unavailable("429".to_owned()).why(), "429");
    }

    #[test]
    fn opensea_rate_limits_get_a_slower_retry() {
        assert_eq!(
            mint_action_retry_delay(&gql::GqlError::Status { status: 429 }),
            Duration::from_secs(5)
        );
        assert_eq!(
            mint_action_retry_delay(&gql::GqlError::Transport("reset".to_owned())),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn sold_out_signed_actions_stop_polling() {
        assert!(mint_action_refusal_is_final(&gql::GqlError::Refused(
            "InsufficientMintsRemainingError".to_owned()
        )));
    }
}
