<p align="left">
  <img src="assets/nock-mark.svg" width="48" alt="Nock mark">
</p>

# Nock CLI

Your wallet. Your machine. Your call.

A self-hosted minter for Robinhood Chain. Your keys, your machine, nobody's
permission.

Robinhood Chain sequences transactions first come, first served. There is no
priority fee, so nobody can outbid anyone and being early is the only edge. This
is the tool for people who would rather run that themselves than trust a service
with a key.

Nock never asks for a seed phrase and never sends a key anywhere. Keys live in a
standard v3 keystore on your disk, decrypted into memory for the length of a run
and wiped afterwards.

This repository is the public home of the Nock CLI and its coding-agent skill.
The orange mark in this README is the Nock mark used across the product.

---

## Coding-agent skill

The repository also ships a coding-agent skill that explains how to install,
use, and extend Nock without weakening its non-custodial safety boundaries.
The skill is available through npm and as a GitHub Release ZIP.

### Install through npm

```bash
npx nock-cli install --agent codex
npx nock-cli install --agent claude
npx nock-cli install --agent cursor
```

Install globally when the installer should remain available:

```bash
npm install --global nock-cli
nock-cli install --agent codex
```

Supported agent targets are `codex`, `claude`, `cursor`, `windsurf`, and
`gemini`. Install into another coding agent with an explicit directory:

```bash
nock-cli install --path ~/.my-agent/skills/nock-cli
```

Pass `--force` to replace an existing installation after reviewing the version.

### What gets downloaded

`npx nock-cli` downloads the Nock package from npm. The package has no runtime
dependencies. It contains the Nock installer, the Nock coding-agent skill, its
implementation reference, and the license. It does not create wallets, copy
keys, install a hosted service, or send credentials anywhere.

The native minter is separate. `cargo build --release` downloads the Rust crates
listed in `Cargo.lock`, then builds the local `nock` binary. Those crates are
build dependencies; they are not Nock services and they never receive wallet
files or transaction credentials.

### Download the skill ZIP

Download `nock-cli-skill.zip` from the
[latest GitHub Release](https://github.com/Iziedking/nock-cli/releases/latest),
extract it, and place the `nock-cli-skill` directory under the agent's skills
directory:

```text
<agent-skills>/nock-cli/SKILL.md
<agent-skills>/nock-cli/references/implementation.md
```

For Codex, the usual user-level location is `~/.codex/skills/nock-cli/`. For
Claude Code, use `~/.claude/skills/nock-cli/`. Project-local agents can use a
project `.cursor/skills/nock-cli/` or another path accepted by that agent.
Start a new agent session after copying the files so the skill is discovered.

The npm installer and release ZIP install the skill only. The native `nock`
minter remains a separate Rust binary and is installed in the section below.

---

## Install the native Nock minter

Needs Rust 1.97.1 or later.

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

The collection can be an address, an OpenSea link or a bare slug, because the
link is what you have when you are looking at a drop:

```bash
nock mint https://opensea.io/collection/mr-machine --wallet wallets/main.json
nock mint mr-machine --wallet wallets/main.json
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

## Last-minute cron protection

If you do not want to be online at the exact opening time, `nock cron` can
watch several stages and make one controlled attempt for each one. It is a
general scheduler, not a Goat Street special case: the collection, stage and
wallet are all supplied in the schedule file.

The scheduler is deliberately conservative:

- it is dry-run unless you add `--fire`
- it starts in the final 60 seconds, or immediately if the stage is already
  open and has not ended
- it records its state and permits one broadcast per job
- before acting, it compares the wallet's pending nonce and NFT balance with a
  pre-window baseline
- activity from an earlier stage is absorbed before the next safety window;
  activity inside the final window makes cron stand down
- for signed stages, a fire run keeps asking OpenSea for the wallet-specific
  mint action through 30 seconds after opening, covering short eligibility lag
- exact transaction simulation also retries through that opening grace period
  before any bytes are broadcast
- a scheduled transaction is never automatically retried, because an accepted
  transaction can be real even when a receipt check is temporarily unavailable

This prevents the scheduler from competing with a manual mint from the same
wallet during the race window without letting a completed earlier stage cancel
the next one. Use a dedicated mint wallet; any transaction made inside the
final window counts as manual activity and safely causes cron to stand down.

Create a schedule such as this one. The stage numbers below match the Goat
Street drop: GTD is stage 2, FCFS is stage 3, and public is stage 0. Other
collections use their own stage numbers.

```json
{
  "poll_seconds": 5,
  "window_seconds": 60,
  "state_file": "/var/lib/nock/goat-street-cron.json",
  "jobs": [
    {
      "id": "goat-street-gtd",
      "collection": "0xc21159f412c294ca2c38f2a9ecaaccf9d93ec929",
      "stage": 2,
      "quantity": 1,
      "wallet": "/var/lib/nock/wallets/goat.json"
    },
    {
      "id": "goat-street-fcfs",
      "collection": "0xc21159f412c294ca2c38f2a9ecaaccf9d93ec929",
      "stage": 3,
      "quantity": 1,
      "wallet": "/var/lib/nock/wallets/goat.json"
    },
    {
      "id": "goat-street-public",
      "collection": "0xc21159f412c294ca2c38f2a9ecaaccf9d93ec929",
      "stage": 0,
      "quantity": 1,
      "wallet": "/var/lib/nock/wallets/goat.json"
    }
  ]
}
```

Run a one-time dry run first:

```bash
nock cron \
  --schedule /etc/nock/goat-street.json \
  --passphrase-file /etc/nock/goat.pass \
  --once
```

Then leave the dry-run scheduler running before the drop. It will arm its
baseline and print the plan in the final minute, but it cannot broadcast:

```bash
nock cron \
  --schedule /etc/nock/goat-street.json \
  --passphrase-file /etc/nock/goat.pass
```

Only after reviewing that test should the production launch command include
`--fire`:

```bash
nock cron \
  --schedule /etc/nock/goat-street.json \
  --passphrase-file /etc/nock/goat.pass \
  --fire
```

The passphrase file is the explicit tradeoff for unattended signing. It must
be readable only by the account running Nock (`chmod 600`) and should live on a
locked machine. Nock reads it into memory for the child mint process and never
puts it in an argument or log. If you do not want an at-rest passphrase file,
run the normal `nock mint` command manually instead.

### Running the CLI on `nock-vm`

The CLI does not use the Telegram bot's hosted wallet. Build the binary and
copy it to the VM, then keep the CLI wallet and cron state in their own
directories:

```bash
# From the repository checkout on your workstation
cargo build --release --manifest-path cli/Cargo.toml
scp cli/target/release/nock nock-vm:/tmp/nock
scp cli/examples/goat-street.json nock-vm:/tmp/goat-street.json

# On nock-vm
sudo install -d -o root -g root -m 0755 /opt/nock
sudo install -o root -g root -m 0755 /tmp/nock /opt/nock/nock
sudo install -d -o nock -g nock -m 0700 /etc/nock
sudo install -o nock -g nock -m 0600 /tmp/goat-street.json /etc/nock/goat-street.json
sudo install -d -o nock -g nock -m 0700 /var/lib/nock/wallets
sudo install -d -o nock -g nock -m 0700 /var/lib/nock
```

Copy the encrypted keystore to `/var/lib/nock/wallets/goat.json`, owned by the
service account. Create `/etc/nock/nock.env` with mode 600 and set the RPC
variables there, for example `NOCK_RPC_URLS` with Alchemy first and the public
Robinhood endpoint second. Keep the Alchemy URL out of shell history and logs.

Create the passphrase file interactively on the VM:

```bash
sudo -u nock sh -c 'umask 077; printf "Passphrase: "; read -r -s p; printf "\\n"; printf "%s\\n" "$p" > /etc/nock/goat.pass; unset p'
sudo chmod 600 /etc/nock/goat.pass
sudo chown nock:nock /etc/nock/goat.pass
```

Test without fire:

```bash
sudo -u nock sh -lc '. /etc/nock/nock.env; /opt/nock/nock doctor'
sudo -u nock sh -lc '. /etc/nock/nock.env; /opt/nock/nock cron --schedule /etc/nock/goat-street.json --passphrase-file /etc/nock/goat.pass --once'
```

For a cron-launched resident scheduler, create `/usr/local/sbin/nock-cron`
with `sudoedit`:

```sh
#!/bin/sh
set -eu
. /etc/nock/nock.env
exec /opt/nock/nock cron \
  --schedule /etc/nock/goat-street.json \
  --passphrase-file /etc/nock/goat.pass
```

Make it executable and launch the dry-run service at boot:

```bash
sudo chmod 0755 /usr/local/sbin/nock-cron
sudo crontab -u nock -e
```

Add this line while testing:

```cron
@reboot /usr/local/sbin/nock-cron >>/var/lib/nock/cron.log 2>&1
```

When the dry-run has been reviewed, add `--fire` to the wrapper. Do not run a
second copy: the state lock refuses duplicate schedulers, and the state file
must remain on persistent disk. A systemd service with `Restart=on-failure` is
preferable if you want automatic restart after a VM process failure; the CLI
behavior and the dry-run/`--fire` boundary are identical.

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
