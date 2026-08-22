---
name: nock-cli
description: This skill should be used when the user asks to "install Nock", "use the Nock CLI", "implement Nock CLI support", "build a Robinhood Chain mint flow", or "add Nock to a coding agent". It guides safe, non-custodial CLI use and Rust implementation work.
---

# Nock CLI

## Purpose

Use this skill to work with the Nock self-hosted CLI for Robinhood Chain, or to
extend the CLI without weakening its custody and mint-safety guarantees. Keep
the native CLI on the user's machine. Keep wallet files, passphrases, private
keys, RPC credentials, and transaction approval outside model context.

Treat the CLI as a self-hosted mint planner and executor. It reads chain state,
builds a bounded plan, shows the plan before spending, and only broadcasts
when the user explicitly supplies `--fire`. A plan is not a transaction and a
dispatch is not a successful mint.

## Operating model

Nock targets Robinhood Chain, chain ID 4663, and the SeaDrop singleton. The
native Rust binary supports:

- `PUBLIC_SALE` stages built from chain data.
- `SIGNED_PRESALE` stages using OpenSea's signed mint route and verification.
- Wallets stored as Web3 Secret Storage v3 keystores.
- One wallet or an ordered wallet-set file.
- A whole-run `--max-spend` ceiling for paid mints.
- A doctor command for RPC reachability and clock drift.

Refuse `MERKLE_PRESALE` stages. Do not substitute a public call when a proof is
missing. Do not claim success for `dispatched`, `rejected`, `vanished`, or
`included but reverted` outcomes.

Nock never requests a seed phrase or a raw private key. A keystore passphrase
may be entered interactively by the local process, but must never appear in
argv, shell history, logs, prompts, test fixtures, or model messages. Never
invent a recipient, fee recipient, quantity, stage, price, or proof.

## Use the CLI safely

Start with the non-mutating checks:

```bash
nock doctor
nock mint <collection-or-opensea-link> --quantity 1 --wallet wallets/main.json
```

Review the printed stage, recipient, price, fee, balance, eligibility, and
spend calculation. For a paid stage, require an explicit whole-run ceiling:

```bash
nock mint <collection> --wallet wallets/main.json \
  --max-spend 0.01 --fire
```

Use `--fire` only after the plan is understood and the user has authorized the
specific run. Treat a non-zero exit code or any partial outcome as a result to
investigate, not as permission to retry blindly. Preserve the output and
transaction hash for reconciliation before attempting a replacement.

For several wallets, use a user-authored ordered file:

```text
wallets/main.json
wallets/second.json
```

```bash
nock mint <collection> --wallet-set wallets.txt --max-spend 0.05 --fire
```

The file order is a user decision. Do not reorder it automatically to optimize
an outcome.

## Implement or modify the CLI

Read `references/implementation.md` before changing Rust code. Preserve the
existing boundaries:

1. Read configuration and endpoint failover through the existing modules.
2. Read and validate chain and OpenSea evidence before constructing calldata.
3. Build a deterministic plan with explicit bounds.
4. Render the plan before any signing or broadcast.
5. Require explicit fire authorization and a whole-run spend cap for money.
6. Send only the exact transaction represented by the reviewed plan.
7. Confirm the outcome from receipts and chain reads rather than trusting a
   returned transaction hash.

Keep the Rust binary free of service credentials and hosted-wallet assumptions.
Keep external calls behind the existing RPC/OpenSea seams. Add fixtures for
new response shapes and unit tests for every refusal path before adding a live
integration test.

When adding a command, document its dry-run behavior, the exact authorization
boundary, exit codes, wallet-file handling, and recovery procedure. Do not add
a flag that accepts a private key, bypasses the spend ceiling, silently chooses
a stage, or converts an unverified response into success.

## Coding-agent workflow

For a request to integrate Nock into another project:

1. Inspect the target repository and identify whether the integration needs the
   native Rust binary, the skill only, or both.
2. Install this skill into the selected agent's skill directory or a custom
   path. Keep the native binary installation separate from skill installation.
3. Read the target project's local instructions before editing files.
4. Propose the exact files, commands, network calls, and money boundaries before
   implementation.
5. Keep planning and preview commands separate from fire commands.
6. Add tests for parsing, calldata invariants, refusals, spend limits, and
   outcome classification.
7. Run formatting, clippy with warnings denied, unit tests, and a release build.
8. Report what was verified locally and what still needs user-controlled live
   evidence. Never claim a live mint without a receipt and recipient proof.

## Additional resources

- `references/implementation.md` contains the Rust module map, invariants, test
  matrix, and release checklist.
