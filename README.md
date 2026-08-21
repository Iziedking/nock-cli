# nock

A self-hosted minter for Robinhood Chain. Your keys, your machine, nobody's
permission.

Robinhood Chain sequences transactions first come, first served. There is no
priority fee, so nobody can outbid anyone and being early is the only edge. This
is the tool for people who would rather run that themselves than trust a service
with a key.

Nock never asks for a seed phrase and never sends a key anywhere. Keys live in a
standard v3 keystore on your disk, decrypted into memory for the length of a run
and wiped afterwards.

---

## Install

Needs Rust 1.90 or later.

```bash
git clone https://github.com/Iziedking/nock-cli
cd nock-cli
cargo build --release
```

The binary lands at `target/release/nock`. It carries its own TLS, so there is
nothing else to install on the machine it runs on.

## Make a wallet

```bash
nock wallets new --path wallets/main.json
```

You choose a passphrase. It is never stored, never sent anywhere and cannot be
recovered, so back the file up somewhere the passphrase is not written down.

It is a Web3 Secret Storage v3 keystore, which means MetaMask, Rabby and geth can
all import it. You are not locked in to this tool.

```bash
nock wallets show --path wallets/main.json     # the address, without unlocking
nock wallets unlock --path wallets/main.json   # check the passphrase opens it
```

Send that address some ETH. Gas on this chain is measured in millionths: a mint
at 320,000 gas costs around 0.0000064 ETH.

## Check the machine

```bash
nock doctor
```

Reports the chain it can reach, how far your clock is from real time and whether
the endpoints answer. Worth running before a drop rather than during one, because
a clock more than 250 ms out will refuse to fire.

## Mint

Look before you leap. Without `--fire` nothing is ever sent:

```bash
nock mint 0xCollectionAddress --quantity 1 --wallet wallets/main.json
```

That prints a plan: which stage, when it opens, what it costs, whether your
wallet covers it, and whether anything is left to mint. Read it, then:

```bash
nock mint 0xCollectionAddress --quantity 1 --wallet wallets/main.json --fire
```

### Paid stages

Anything with a price needs a ceiling, and the ceiling is for the whole run
rather than per stage or per wallet:

```bash
nock mint 0xCollection --quantity 2 --wallet wallets/main.json --max-spend 0.01 --fire
```

`--fire` alone stops being enough authorisation once money can move, because a
price can rise between planning and firing and there is nobody to ask mid-run.

### Several wallets at once

An allowlist campaign is usually several wallets on one list. Put their keystore
paths in a file, one per line:

```
# wallets.txt
wallets/main.json
wallets/second.json
wallets/third.json
```

```bash
nock mint 0xCollection --wallet-set wallets.txt --max-spend 0.05 --fire
```

One passphrase unlocks the set, and every wallet is sent at the same moment
rather than one after another.

**The order of that file matters.** If a price rises and the run can no longer
afford everybody, wallets are dropped from the bottom. That way the decision
about who loses their place is one you made in advance rather than one the tool
makes under time pressure.

### Picking a stage

A drop usually has several stages: an allowlist, then public. Without `--stage`
the run takes the earliest one that has not ended.

```bash
nock mint 0xCollection --stage 2 --wallet wallets/main.json
```

## What it can and cannot mint

| Stage | Supported |
|---|---|
| `PUBLIC_SALE` | yes, built entirely from chain data |
| `SIGNED_PRESALE` | yes, this is what "allowlist" and "FCFS" mean on this chain |
| `MERKLE_PRESALE` | no, refused rather than attempted |

Merkle allowlists need a proof this tool does not build. Measured on chain 4663,
50 of 52 collections gate with a signer instead, so this refuses a rounding error
rather than a market. A merkle stage is named and skipped, never quietly minted
as a public one.

## Allowlist mints, and what OpenSea has to do with it

A signed stage needs a signature produced by the collection's signer key, which
OpenSea holds. It cannot be derived, read off chain or computed, so there is no
version of allowlist minting without asking them for it.

So the tool signs in with your wallet, asks whether you are on the list, and asks
for the calldata. Then it checks that calldata against everything it already
knows before a key touches it:

- the call goes to the SeaDrop singleton and nowhere else
- the selector matches the kind of stage you are entering
- the collection is the one you asked for
- **the token goes to your wallet**, not somewhere else
- the quantity and the unit price are what you were quoted
- the value is price times quantity
- the fee recipient is one the collection allows
- the price, quantity, window and fee sit inside the bounds the collection
  published on chain

Any one of those failing refuses that wallet, names the field, prints both
values, and lets the others carry on. Nothing is guessed and nothing is silently
dropped.

Signing in costs one signature. It proves you own the address and moves nothing.

## Reading the output

Before firing, every wallet gets a line with its status: `ready`,
`not eligible`, `underfunded`, `sold out`, `refused` or `dropped for spend`, with
the arithmetic behind it.

After firing, every wallet gets one of four outcomes and no fifth:

| | |
|---|---|
| `minted` | a receipt exists and it did not revert |
| `included but reverted` | it landed and did nothing. Not a win |
| `rejected` | no endpoint would take it |
| `vanished` | an endpoint accepted it and it never appeared |
| `dispatched, no receipt yet` | sent, unconfirmed. **Not a win** |

A dispatch is never dressed up as success. Exit code is 0 if at least one wallet
minted and 1 if none did, so partial success across a set reads as success,
because three of eight minting is three more than not running.

## Configuration

Everything has a working default. Set these only if you need to:

| Variable | Default |
|---|---|
| `NOCK_RPC_URLS` | the public Robinhood Chain endpoint, comma separated for failover |
| `NOCK_SEQUENCER_URL` | where transactions are sent |
| `NOCK_CHAIN_ID` | 4663 |

Endpoints are tried in the order you list them. The first is the primary and the
rest are failover, so put the one you trust most first.

## What this tool will not do

- Ask for a seed phrase or a private key. It reads a keystore you made.
- Send a token anywhere but the wallet that minted it.
- Spend past `--max-spend`.
- Mint a stage it cannot verify.
- Call a dispatch a mint.

## Verifying it yourself

The derivation used by the hosted Nock wallet is written up in
[docs/nock-wallet.md](docs/nock-wallet.md), including how to compute the address
independently and confirm it matches.

## Licence

See [LICENSE](LICENSE).
