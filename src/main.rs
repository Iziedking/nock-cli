//! nock, a self-hosted minter for Robinhood Chain.
//!
//! Your keys, your machine, nobody's permission. No collection has to enable
//! anything and no service ever sees a key or a coin.

mod chain;
mod commands;
mod config;
mod engine;
mod plan;
mod wallet;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "nock",
    bin_name = "nock",
    version,
    about = "Self-hosted NFT minter for Robinhood Chain",
    long_about = "Mint for yourself on Robinhood Chain. Your keys never leave this machine.\n\n\
                  Configuration comes from the environment, never from a flag, because a private \
                  key on a command line reaches shell history and process listings.\n\n\
                  \x20 NOCK_RPC_URLS        comma separated, first is preferred\n\
                  \x20 NOCK_SEQUENCER_URL   send-only endpoint\n\
                  \x20 NOCK_PRIVATE_KEY     required to sign anything"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check the chain, the sequencer, the clock and the wallet before you need them.
    Doctor,
    /// Mint a public stage for yourself. Prints what it would do unless --fire.
    Mint {
        /// The NFT contract address.
        collection: String,
        /// How many to mint, within the stage's per wallet cap.
        #[arg(long, short, default_value_t = 1)]
        quantity: u64,
        #[arg(long, short)]
        wallet: Option<PathBuf>,
        /// Actually send it. Without this nothing is broadcast.
        #[arg(long)]
        fire: bool,
        /// The most this run may spend on mint prices, in ETH, for example
        /// 0.05. Required when the stage is not free.
        #[arg(long, value_name = "ETH")]
        max_spend: Option<String>,
    },
    /// Create and inspect the encrypted wallet this machine mints with.
    Wallets {
        #[command(subcommand)]
        command: WalletCommand,
    },
}

#[derive(Debug, Subcommand)]
enum WalletCommand {
    /// Create a new wallet, encrypted with a passphrase you choose.
    New {
        /// Where to write it. An existing file is never overwritten.
        #[arg(long, short)]
        path: Option<PathBuf>,
    },
    /// Print the address a wallet holds, without unlocking it.
    Show {
        #[arg(long, short)]
        path: Option<PathBuf>,
    },
    /// Check that a passphrase opens a wallet. Prints nothing secret.
    Unlock {
        #[arg(long, short)]
        path: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration problem: {err}");
            return std::process::ExitCode::from(2);
        }
    };

    match cli.command {
        Command::Doctor => commands::doctor::run(&config).await,
        Command::Mint {
            collection,
            quantity,
            wallet,
            fire,
            max_spend,
        } => {
            let max_spend_wei = match max_spend.as_deref().map(plan::spend::parse_eth) {
                Some(Ok(wei)) => Some(wei),
                Some(Err(message)) => {
                    eprintln!(
                        "
  --max-spend: {message}
"
                    );
                    return std::process::ExitCode::FAILURE;
                }
                None => None,
            };
            commands::mint::run(
                &config,
                commands::mint::MintArgs {
                    collection: &collection,
                    max_spend_wei,
                    quantity,
                    wallet: &wallet.unwrap_or_else(commands::wallets::default_path),
                    fire,
                },
            )
            .await
        }
        Command::Wallets { command } => {
            let at = |p: Option<PathBuf>| p.unwrap_or_else(commands::wallets::default_path);
            match command {
                WalletCommand::New { path } => commands::wallets::new_wallet(&at(path)),
                WalletCommand::Show { path } => commands::wallets::show(&at(path)),
                WalletCommand::Unlock { path } => commands::wallets::unlock(&at(path)),
            }
        }
    }
}
