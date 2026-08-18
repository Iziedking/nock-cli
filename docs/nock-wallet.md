# Working out your Nock wallet address

If you use the Nock bot, it mints into an address that belongs to you and that
Nock cannot take anything out of. This page is how you check that yourself
instead of taking our word for it.

Your address is **derived, not assigned**. Anyone can compute it, which means you
can confirm the one the bot gave you is the one the contracts will actually
deploy to.

## The three addresses it comes from

    factory        0x4BBf69d5b882fDA14903B2b04886AaE5D028A2b2
    implementation 0x3e00521648b09726D662A259450E908C75Bb0651
    dispatcher     0x4Cf29825303898FaC7cb2778f19CE15E1C3bD545

Chain 4663, Robinhood Chain. All three are verified on the explorer, so you can
read the code rather than the description of it.

## Work out your own

```bash
cast call <factory> "minterOf(address)(address)" <your address> \
  --rpc-url https://rpc.mainnet.chain.robinhood.com
```

That answers before the account exists. The address is a CREATE2 address worked
out in advance, which is what lets you register it for an allowlist weeks before
anybody deploys anything.

## What you can check about it

Your own address and the dispatcher are written into the account's own bytecode,
so the address itself commits to who controls it. Two people cannot share an
account, and no account can be deployed at your address that answers to somebody
else.

Once it exists:

```bash
cast call <your nock wallet> "owner()(address)"      --rpc-url ...   # you
cast call <your nock wallet> "dispatcher()(address)" --rpc-url ...   # NockBatch
```

Read the source and you will find there is no `execute`, no upgrade path, no
initialiser, and no way to change the owner. Not as a promise: there is no
function that does it.

## Getting your money out

```bash
cast send <your nock wallet> "sweep()" \
  --rpc-url https://rpc.mainnet.chain.robinhood.com
```

`sweep()` sends the whole balance to `owner()`, which is you, and nowhere else.
It is callable by **anyone**, which sounds wrong until you notice what it means:
the caller cannot choose the destination, so a stranger calling it can only push
your money home. If Nock disappeared tomorrow your funds would not be stuck, and
you would not need us to release them.

`rescueERC721` and `rescueERC20` do the same for tokens, and those are owner
only.

## Why it is a contract and not a seed phrase

There is nothing to import into MetaMask, because there is no private key. That
is the trade: a contract account cannot sign messages, so it cannot claim an
airdrop that requires a signature. In exchange, Nock can mint for you without
ever holding a key of yours, and there is no key of yours for anyone to lose.

Your NFT does not stay in it either. The account forwards each token to your own
wallet in the same transaction it mints, so anything that snapshots holders sees
your real address.

## What the account can and cannot do

| | |
|---|---|
| You | mint through it, sweep it, rescue anything in it |
| Nock | ask it to mint, with the token going to you and nowhere else |
| Anyone | call `sweep()`, which pays you |
| Nobody | change the owner, withdraw to another address, or upgrade it |

On a paid stage the mint price comes out of this account's own balance, which you
funded. Nock supplies the gas and the ordering slot and never fronts or holds the
money.
