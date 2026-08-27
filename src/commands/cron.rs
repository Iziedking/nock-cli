//! A small, persistent last-minute scheduler for unattended drops.
//!
//! The scheduler is deliberately separate from the Telegram bot. It watches
//! the same chain and calls the normal `mint` command for one controlled
//! attempt. Dry-run is the default; `--fire` is an explicit second decision.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::chain::rpc::{parse_hex_u64, Rpc};
use crate::commands::mint::{choose_stage, resolve_collection, wallet_paths};
use crate::config::Config;
use crate::wallet::keystore::Keystore;

const DEFAULT_WINDOW_SECONDS: u64 = 60;
const DEFAULT_POLL_SECONDS: u64 = 5;
const STAGE_REFRESH_SECONDS: u64 = 15;

#[derive(Debug, PartialEq, Eq)]
enum StageWindow {
    Before,
    Open,
    Ended,
}

fn stage_window(now: u64, start_time: u64, end_time: u64) -> StageWindow {
    if now < start_time {
        StageWindow::Before
    } else if now < end_time {
        StageWindow::Open
    } else {
        StageWindow::Ended
    }
}

#[derive(Debug)]
pub struct CronArgs {
    pub schedule: PathBuf,
    pub fire: bool,
    pub once: bool,
    pub interval: u64,
    pub passphrase_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ScheduleFile {
    #[serde(default)]
    poll_seconds: Option<u64>,
    #[serde(default)]
    window_seconds: Option<u64>,
    #[serde(default)]
    state_file: Option<PathBuf>,
    jobs: Vec<CronJob>,
}

#[derive(Debug, Deserialize)]
struct CronJob {
    id: String,
    collection: String,
    stage: u64,
    #[serde(default = "one")]
    quantity: u64,
    wallet: Option<PathBuf>,
    wallet_set: Option<PathBuf>,
    max_spend: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CronState {
    #[serde(default)]
    jobs: BTreeMap<String, JobState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JobState {
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    stage: Option<StageSnapshot>,
    #[serde(default)]
    checked_at: u64,
    #[serde(default)]
    collection_address: Option<String>,
    #[serde(default)]
    baseline: Vec<WalletBaseline>,
    #[serde(default)]
    dry_run_reported: bool,
    #[serde(default)]
    attempted: bool,
    #[serde(default)]
    last_action: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StageSnapshot {
    start_time: u64,
    end_time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletBaseline {
    path: String,
    address: String,
    pending_nonce: u64,
    nft_balance: u64,
}

#[derive(Debug)]
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone)]
struct ChildResult {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

struct CronContext<'a> {
    config: &'a Config,
    http: &'a reqwest::Client,
    base: &'a Path,
    state_path: &'a Path,
    window_seconds: u64,
    fire: bool,
    passphrase: Option<&'a Zeroizing<String>>,
}

pub async fn run(config: &Config, args: CronArgs) -> std::process::ExitCode {
    match run_inner(config, args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("\n  {message}\n");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_inner(config: &Config, args: CronArgs) -> Result<(), String> {
    let schedule_path = absolute_path(&args.schedule)?;
    let base = schedule_path
        .parent()
        .ok_or_else(|| "the schedule has no parent directory".to_owned())?;
    let spec = read_schedule(&schedule_path)?;
    validate_schedule(&spec)?;

    let state_path = spec.state_file.as_ref().map_or_else(
        || schedule_path.with_extension("state.json"),
        |path| resolve_path(base, path),
    );
    if let Some(parent) = state_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create state directory {}: {e}", parent.display()))?;
    }
    let lock = acquire_lock(&state_path.with_extension("lock"))?;
    let passphrase = load_passphrase(args.passphrase_file.as_deref())?;
    if passphrase.is_none() {
        return Err(
            "cron requires --passphrase-file when stdin is not an interactive terminal".to_owned(),
        );
    }
    let poll_seconds = if args.interval == DEFAULT_POLL_SECONDS {
        spec.poll_seconds.unwrap_or(args.interval)
    } else {
        args.interval
    }
    .max(1);
    let window_seconds = spec.window_seconds.unwrap_or(DEFAULT_WINDOW_SECONDS).max(1);

    println!(
        "Nock cron: {} job(s), {} second window, {} second poll, {}",
        spec.jobs.len(),
        window_seconds,
        poll_seconds,
        if args.fire { "FIRE ENABLED" } else { "DRY RUN" }
    );
    if args.fire {
        println!("Broadcasting is enabled. Each job has one attempt and will not be retried automatically.");
    } else {
        println!("No transaction can be broadcast in this mode. Add --fire only after reviewing the dry run.");
    }

    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 nock")
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;
    let mut state = read_state(&state_path)?;

    loop {
        let mut changed = false;
        let now = now_unix();
        let context = CronContext {
            config,
            http: &http,
            base,
            state_path: &state_path,
            window_seconds,
            fire: args.fire,
            passphrase: passphrase.as_ref(),
        };
        for job in &spec.jobs {
            changed |= process_job(&context, job, &mut state, now).await?;
        }
        if changed {
            write_state(&state_path, &state)?;
        }
        if args.once {
            break;
        }
        tokio::time::sleep(Duration::from_secs(poll_seconds)).await;
        refresh_lock(&lock.path)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn process_job(
    context: &CronContext<'_>,
    job: &CronJob,
    state: &mut CronState,
    now: u64,
) -> Result<bool, String> {
    let target_paths = target_paths(job, context.base)?;
    let target_addresses = wallet_addresses(&target_paths)?;
    let job_state = state.jobs.entry(job.id.clone()).or_default();
    let fingerprint = job_fingerprint(job, &target_paths);
    if job_state.fingerprint.as_deref() != Some(fingerprint.as_str()) {
        *job_state = JobState {
            fingerprint: Some(fingerprint),
            ..JobState::default()
        };
    }

    let stage_refreshed = !(job_state.stage.is_some()
        && now.saturating_sub(job_state.checked_at) < STAGE_REFRESH_SECONDS);
    let window = if stage_refreshed {
        let (collection, slug) = resolve_collection(context.http, &job.collection).await?;
        let (selected_stage, _) = choose_stage(
            &mut Rpc::new(context.config.rpc_urls.clone(), Duration::from_secs(10)),
            context.http,
            collection,
            Some(job.stage),
            slug,
        )
        .await?;
        job_state.collection_address = Some(format!("{collection:?}"));
        job_state.checked_at = now;
        let snapshot = StageSnapshot {
            start_time: selected_stage.start_time,
            end_time: selected_stage.end_time,
        };
        job_state.stage = Some(snapshot.clone());
        snapshot
    } else {
        let cached = job_state
            .stage
            .as_ref()
            .ok_or_else(|| "cron stage cache disappeared".to_owned())?;
        StageSnapshot {
            start_time: cached.start_time,
            end_time: cached.end_time,
        }
    };

    let mut changed = stage_refreshed;
    if !baseline_matches(&job_state.baseline, &target_paths, &target_addresses) {
        job_state.baseline =
            capture_baseline(context.config, &job.collection, &target_paths).await?;
        changed = true;
        println!(
            "  [{}] armed for stage {} at {}",
            job.id, job.stage, window.start_time
        );
        // A schedule first seen inside the last-minute window is not safe to
        // fire: it has no known pre-window baseline against which to detect a
        // manual transaction.
        if now.saturating_add(context.window_seconds) >= window.start_time {
            println!(
                "  [{}] baseline created inside the safety window; waiting for a future run",
                job.id
            );
            return Ok(changed);
        }
    }

    if job_state.attempted || (!context.fire && job_state.dry_run_reported) {
        return Ok(changed);
    }

    match stage_window(now, window.start_time, window.end_time) {
        StageWindow::Ended => {
            job_state.attempted = true;
            job_state.last_action = Some("stage_ended".to_owned());
            println!("  [{}] ended without a scheduled attempt", job.id);
            return Ok(true);
        }
        // A stage can open earlier than the published timestamp, or the
        // machine can be restarted after the last-minute window. The contract
        // and the live mint simulation are the authority in that case. The
        // baseline check above still prevents a manual mint from being
        // duplicated.
        StageWindow::Before => {
            let seconds_until_open = window.start_time.saturating_sub(now);
            if seconds_until_open > context.window_seconds {
                return Ok(changed);
            }
        }
        StageWindow::Open => {
            println!(
                "  [{}] stage is already open; checking for a safe mint now",
                job.id
            );
        }
    }

    let current = capture_baseline(context.config, &job.collection, &target_paths).await?;
    if manual_activity(&job_state.baseline, &current) {
        job_state.attempted = true;
        job_state.last_action = Some("manual_activity_detected".to_owned());
        println!(
            "  [{}] manual wallet activity detected; cron will not clash",
            job.id
        );
        let collection_address = job_state.collection_address.clone().unwrap_or_default();
        sync_related_baselines(state, &collection_address, &target_addresses, &current);
        return Ok(true);
    }

    let passphrase = context.passphrase.ok_or_else(|| {
        "cron needs --passphrase-file (or an interactive terminal) to unlock the scheduled wallet".to_owned()
    })?;
    if context.fire {
        // Persist before starting the child. If the process dies after the
        // transaction is accepted, a restart must not send a second one.
        job_state.attempted = true;
        job_state.last_action = Some("fire_started".to_owned());
        let _ = job_state;
        write_state(context.state_path, state)?;
    }
    let child = run_mint_child(context.base, job, &target_paths, context.fire, passphrase).await?;
    if !child.stdout.trim().is_empty() {
        print!("{}", child.stdout);
    }
    if !child.stderr.trim().is_empty() {
        eprint!("{}", child.stderr);
    }

    if context.fire {
        // Persist again with the observed child result. The attempted marker
        // was already durable before the child started.
        let job_state = state
            .jobs
            .get_mut(&job.id)
            .ok_or_else(|| format!("cron job {} disappeared from state", job.id))?;
        job_state.last_action = Some(format!("fire_exit_{:?}", child.status));
        let collection_address = job_state.collection_address.clone().unwrap_or_default();
        let post_attempt =
            match capture_baseline(context.config, &job.collection, &target_paths).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    eprintln!(
                        "  [{}] could not refresh the post-attempt nonce baseline: {error}",
                        job.id
                    );
                    current
                }
            };
        sync_related_baselines(state, &collection_address, &target_addresses, &post_attempt);
        println!(
            "  [{}] scheduled attempt finished; inspect the child outcome before retrying",
            job.id
        );
    } else {
        let job_state = state
            .jobs
            .get_mut(&job.id)
            .ok_or_else(|| format!("cron job {} disappeared from state", job.id))?;
        job_state.dry_run_reported = true;
        job_state.last_action = Some(format!("dry_run_exit_{:?}", child.status));
        println!(
            "  [{}] dry run complete; no transaction was broadcast",
            job.id
        );
    }
    Ok(true)
}

async fn run_mint_child(
    base: &Path,
    job: &CronJob,
    target_paths: &[PathBuf],
    fire: bool,
    passphrase: &Zeroizing<String>,
) -> Result<ChildResult, String> {
    let executable = std::env::current_exe()
        .map_err(|e| format!("could not locate the nock executable: {e}"))?;
    let mut command = Command::new(executable);
    command
        .arg("mint")
        .arg(&job.collection)
        .arg("--quantity")
        .arg(job.quantity.to_string())
        .arg("--stage")
        .arg(job.stage.to_string());
    if let Some(path) = job.wallet.as_ref() {
        command.arg("--wallet").arg(resolve_path(base, path));
    } else if let Some(path) = job.wallet_set.as_ref() {
        command.arg("--wallet-set").arg(resolve_path(base, path));
    } else {
        let path = target_paths
            .first()
            .ok_or_else(|| format!("cron job {} has no wallet", job.id))?;
        command.arg("--wallet").arg(path);
    }
    if let Some(max_spend) = &job.max_spend {
        command.arg("--max-spend").arg(max_spend);
    }
    if fire {
        command.arg("--fire");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the mint child: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "could not open the mint child's stdin".to_owned())?;
    stdin
        .write_all(passphrase.as_bytes())
        .await
        .map_err(|e| format!("could not provide the passphrase to the mint child: {e}"))?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(|e| format!("could not finish the passphrase input: {e}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("the mint child did not finish: {e}"))?;
    Ok(ChildResult {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn capture_baseline(
    config: &Config,
    collection_input: &str,
    paths: &[PathBuf],
) -> Result<Vec<WalletBaseline>, String> {
    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 nock")
        .build()
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;
    let (collection, _) = resolve_collection(&http, collection_input).await?;
    let mut rpc = Rpc::new(config.rpc_urls.clone(), Duration::from_secs(10));
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let store = Keystore::load(path)
            .map_err(|e| format!("could not read wallet {}: {e}", path.display()))?;
        let address: Address = store
            .address()
            .parse()
            .map_err(|_| format!("wallet {} has an invalid address", path.display()))?;
        let nonce: String = rpc
            .call(
                "eth_getTransactionCount",
                json!([format!("{address:?}"), "pending"]),
            )
            .await
            .map_err(|e| format!("could not read the pending nonce for {address:?}: {e}"))?;
        let balance: String = rpc
            .call(
                "eth_call",
                json!([{
                    "to": format!("{collection:?}"),
                    "data": format!("0x70a08231{}", address_word(address))
                }, "latest"]),
            )
            .await
            .map_err(|e| format!("could not read the NFT balance for {address:?}: {e}"))?;
        out.push(WalletBaseline {
            path: path.display().to_string(),
            address: format!("{address:?}"),
            pending_nonce: parse_hex_u64(&nonce).map_err(|e| e.to_string())?,
            nft_balance: parse_hex_u64(&balance).map_err(|e| e.to_string())?,
        });
    }
    Ok(out)
}

fn manual_activity(before: &[WalletBaseline], after: &[WalletBaseline]) -> bool {
    before.iter().any(|old| {
        after
            .iter()
            .find(|current| current.address == old.address)
            .is_some_and(|current| {
                current.pending_nonce > old.pending_nonce || current.nft_balance > old.nft_balance
            })
    })
}

fn sync_related_baselines(
    state: &mut CronState,
    collection_input: &str,
    addresses: &[String],
    current: &[WalletBaseline],
) {
    for job in state.jobs.values_mut() {
        let same_collection = job.collection_address.as_deref() == Some(collection_input);
        let same_wallets = job
            .baseline
            .iter()
            .map(|wallet| wallet.address.clone())
            .eq(addresses.iter().cloned());
        if same_collection && same_wallets {
            job.baseline = current.to_vec();
        }
    }
}

fn baseline_matches(before: &[WalletBaseline], paths: &[PathBuf], addresses: &[String]) -> bool {
    before.len() == paths.len()
        && before
            .iter()
            .zip(paths.iter().zip(addresses.iter()))
            .all(|(old, (path, address))| {
                old.path == path.display().to_string() && old.address == *address
            })
}

fn target_paths(job: &CronJob, base: &Path) -> Result<Vec<PathBuf>, String> {
    let wallet = job.wallet.as_ref().map(|path| resolve_path(base, path));
    let wallet_set = job.wallet_set.as_ref().map(|path| resolve_path(base, path));
    wallet_paths(wallet.as_ref(), wallet_set.as_ref())
}

fn job_fingerprint(job: &CronJob, paths: &[PathBuf]) -> String {
    let wallets = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}|{}|{}|{}|{}",
        job.collection,
        job.stage,
        job.quantity,
        job.max_spend.as_deref().unwrap_or(""),
        wallets
    )
}

fn wallet_addresses(paths: &[PathBuf]) -> Result<Vec<String>, String> {
    paths
        .iter()
        .map(|path| {
            let store = Keystore::load(path)
                .map_err(|e| format!("could not read wallet {}: {e}", path.display()))?;
            let address: Address = store
                .address()
                .parse()
                .map_err(|_| format!("wallet {} has an invalid address", path.display()))?;
            Ok(format!("{address:?}"))
        })
        .collect()
}

fn address_word(address: Address) -> String {
    format!("{}{}", "0".repeat(24), hex::encode(address.as_slice()))
}

fn read_schedule(path: &Path) -> Result<ScheduleFile, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("could not read schedule {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("schedule {} is not valid JSON: {e}", path.display()))
}

fn validate_schedule(spec: &ScheduleFile) -> Result<(), String> {
    if spec.jobs.is_empty() {
        return Err("the cron schedule has no jobs".to_owned());
    }
    let mut ids = std::collections::BTreeSet::new();
    for job in &spec.jobs {
        if job.id.trim().is_empty() || !ids.insert(&job.id) {
            return Err(format!("cron job id {:?} is empty or duplicated", job.id));
        }
        if job.quantity == 0 {
            return Err(format!("cron job {} must mint at least one item", job.id));
        }
        if job.wallet.is_some() == job.wallet_set.is_some() {
            return Err(format!(
                "cron job {} must set exactly one of wallet or wallet_set",
                job.id
            ));
        }
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<CronState, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("cron state {} is not valid JSON: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CronState::default()),
        Err(error) => Err(format!(
            "could not read cron state {}: {error}",
            path.display()
        )),
    }
}

fn write_state(path: &Path, state: &CronState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|e| format!("could not encode cron state: {e}"))?;
    let temporary = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|e| format!("could not write cron state {}: {e}", temporary.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("could not flush cron state: {e}"))?;
    }
    fs::rename(&temporary, path)
        .map_err(|e| format!("could not replace cron state {}: {e}", path.display()))
}

fn acquire_lock(path: &Path) -> Result<LockGuard, String> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            writeln!(file, "pid={} started={}", std::process::id(), now_unix())
                .map_err(|e| format!("could not initialize cron lock: {e}"))?;
            Ok(LockGuard {
                path: path.to_owned(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let stale = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > Duration::from_mins(5));
            if stale {
                fs::remove_file(path).map_err(|remove_error| {
                    format!(
                        "cron lock {} is stale but could not be removed: {remove_error}",
                        path.display()
                    )
                })?;
                return acquire_lock(path);
            }
            Err(format!(
                "another nock cron process owns {}; stop it before starting another scheduler",
                path.display()
            ))
        }
        Err(error) => Err(format!(
            "could not create cron lock {}: {error}",
            path.display()
        )),
    }
}

fn refresh_lock(path: &Path) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| format!("could not refresh cron lock: {e}"))?;
    writeln!(file, "pid={} heartbeat={}", std::process::id(), now_unix())
        .map_err(|e| format!("could not refresh cron lock: {e}"))
}

fn load_passphrase(path: Option<&Path>) -> Result<Option<Zeroizing<String>>, String> {
    if let Some(path) = path {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path)
                .map_err(|e| format!("could not inspect passphrase file {}: {e}", path.display()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(format!(
                    "passphrase file {} must be readable only by its owner (chmod 600)",
                    path.display()
                ));
            }
        }
        let value = fs::read_to_string(path)
            .map_err(|e| format!("could not read passphrase file {}: {e}", path.display()))?;
        return Ok(Some(Zeroizing::new(value.trim_end().to_owned())));
    }
    if std::io::stdin().is_terminal() {
        let value = rpassword::prompt_password("Passphrase for cron wallets: ")
            .map_err(|_| "could not read the cron passphrase".to_owned())?;
        Ok(Some(Zeroizing::new(value)))
    } else {
        Ok(None)
    }
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|e| format!("could not resolve {}: {e}", path.display()))
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

const fn one() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_activity_is_detected_by_nonce_or_nft_balance() {
        let before = vec![WalletBaseline {
            path: "wallet.json".to_owned(),
            address: "0x0000000000000000000000000000000000000001".to_owned(),
            pending_nonce: 4,
            nft_balance: 2,
        }];
        let mut after = before.clone();
        assert!(!manual_activity(&before, &after));
        after[0].pending_nonce = 5;
        assert!(manual_activity(&before, &after));
        after[0].pending_nonce = 4;
        after[0].nft_balance = 3;
        assert!(manual_activity(&before, &after));
    }

    #[test]
    fn an_open_stage_remains_attemptable() {
        assert_eq!(stage_window(110, 100, 200), StageWindow::Open);
        assert_eq!(stage_window(100, 100, 200), StageWindow::Open);
    }

    #[test]
    fn a_future_stage_is_not_open_and_an_ended_stage_is_closed() {
        assert_eq!(stage_window(99, 100, 200), StageWindow::Before);
        assert_eq!(stage_window(200, 100, 200), StageWindow::Ended);
    }

    #[test]
    fn invalid_schedule_requires_one_wallet_source() {
        let spec = ScheduleFile {
            poll_seconds: None,
            window_seconds: None,
            state_file: None,
            jobs: vec![CronJob {
                id: "one".to_owned(),
                collection: "0x0000000000000000000000000000000000000001".to_owned(),
                stage: 0,
                quantity: 1,
                wallet: None,
                wallet_set: None,
                max_spend: None,
            }],
        };
        assert!(validate_schedule(&spec).is_err());
    }
}
