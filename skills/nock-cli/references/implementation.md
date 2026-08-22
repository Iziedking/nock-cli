# Nock CLI implementation reference

This reference is for coding agents changing the Rust CLI. Keep the entrypoint
skill loaded for the custody rules and read this file when implementation work
starts.

## Repository map

| Area | Responsibility |
|---|---|
| `src/main.rs` | Clap command surface and authorization flags |
| `src/config.rs` | Environment defaults and RPC configuration |
| `src/chain/rpc.rs` | HTTP JSON-RPC transport and endpoint failover |
| `src/chain/seadrop.rs` | SeaDrop calldata, public-drop reads, and bounds |
| `src/chain/opensea/` | Collection discovery, SIWE, signed-stage calldata, and verification |
| `src/plan/` | Stage selection, mint planning, and spend arithmetic |
| `src/wallet/` | Web3 Secret Storage v3 keystores and ordered wallet sets |
| `src/engine/` | Clock checks, dispatch, receipt confirmation, and outcomes |
| `src/commands/` | User-facing doctor, mint, report, and wallet commands |
| `tests/fixtures/opensea/` | Redacted network response fixtures |

Keep the dependency set exact. Rust version and lint settings are defined in
`rust-toolchain.toml` and `Cargo.toml`. Avoid adding a provider abstraction or a
second signing implementation when the existing RPC and wallet seams suffice.

## Required invariants

Preserve these invariants in code and tests:

- Chain ID remains 4663 unless a deliberately reviewed multi-chain design is
  introduced.
- The collection, SeaDrop singleton, fee recipient, stage window, quantity,
  unit price, total value, and recipient must agree across every evidence
  source used to build a transaction.
- A signed-stage call must be checked against the OpenSea response and the
  public on-chain bounds before broadcast.
- A Merkle stage must be refused when no proof builder exists.
- A paid run must require `--max-spend`; the ceiling applies to the whole run,
  not only one wallet or one stage.
- A wallet-set is executed in file order. Never reorder it implicitly.
- The default command is a plan. Only `--fire` permits dispatch.
- A transaction hash is transport evidence, not mint evidence. Confirm the
  receipt and recipient transfer before reporting `minted`.
- A keystore passphrase is interactive input only. Never accept raw key material
  through command-line arguments or environment variables.

## Test matrix

For a planner or calldata change, cover:

1. Valid public stage with exact quantity and fee.
2. Stage selection across open, future, ended, and unsupported stages.
3. Price, quantity, supply, wallet-balance, and spend-cap refusals.
4. Wrong collection, recipient, fee recipient, selector, or calldata shape.
5. OpenSea ambiguity, missing response, signed-stage mismatch, and stale SIWE.
6. RPC failover, malformed JSON-RPC, returned-hash mismatch, timeout, and
   confirmation without a receipt.
7. Wallet encryption, wrong passphrase, missing file, ordered wallet sets, and
   zeroization-sensitive paths.
8. Partial wallet-set outcomes and exit-code behavior.

Use checked-in fixtures for deterministic tests. Mark live-network tests
explicitly and keep them out of the default suite unless the repository's
existing test contract says otherwise.

## Release checklist

Run the following from the CLI directory:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
npm test
npm run pack:check
npm run package:skill
```

Inspect the npm dry-run file list. The package must contain the installer,
skill, references, README, and license, and must not contain `target/`, wallet
files, `.env` files, or release output. Inspect the ZIP listing and checksum
before attaching the assets to a GitHub Release.
