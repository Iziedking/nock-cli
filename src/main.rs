//! nock, a self-hosted minter for Robinhood Chain.
//!
//! Your keys, your machine, nobody's permission. No collection has to enable
//! anything and no service ever sees a key or a coin.

mod chain;
mod commands;
mod config;
mod engine;

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
    }
}
