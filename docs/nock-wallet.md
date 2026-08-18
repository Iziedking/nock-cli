# Your Nock wallet

The Nock bot mints into an address that belongs to you. This page is how to
check that yourself.

The address is derived, not assigned. You can work it out and confirm the one
the bot gave you matches what the contracts will actually deploy.

## The addresses it comes from

    factory        0x4BBf69d5b882fDA14903B2b04886AaE5D028A2b2
    implementation 0x3e00521648b09726D662A259450E908C75Bb0651
    dispatcher     0x4Cf29825303898FaC7cb2778f19CE15E1C3bD545

Chain 4663, Robinhood Chain. All three are verified on the explorer.

## Work out your own

```bash
cast call <factory> "minterOf(address)(address)" <your address> \
  --rpc-url https://rpc.mainnet.chain.robinhood.com
```

This answers before the account exists. It is a CREATE2 address, computed ahead
of time, so you can register it for an allowlist weeks before anything is
deployed there.

It will show no code until your first mint. Anything you send it in the meantime
is still there when it deploys.

## Checking it

Your address and the dispatcher are baked into the account's bytecode. Two
people cannot share an account, and nobody can deploy an account at your address
that answers to someone else.

Once it exists:

```bash
cast call <your nock wallet> "owner()(address)"      --rpc-url ...   # you
cast call <your nock wallet> "dispatcher()(address)" --rpc-url ...   # NockBatch
```

The source has no `execute`, no upgrade path, no initialiser and no setter for
the owner.

## Getting your money out

```bash
cast send <your nock wallet> "sweep()" \
  --rpc-url https://rpc.mainnet.chain.robinhood.com
```

`sweep()` sends the balance to `owner()`, which is you. The function takes no
destination argument, so there is nowhere else for it to go.

Anyone can call it, including us and including strangers. That is deliberate: if
Nock goes away, you do not need us to release your money.

`rescueERC721` and `rescueERC20` move tokens out. Both are owner only.

## Why it is a contract and not a seed phrase

There is no private key, so there is nothing to import into MetaMask.

That costs you something. A contract cannot sign a message, so it cannot claim
an airdrop that needs one. In exchange, Nock mints for you without ever holding
a key of yours.

Your NFT does not sit in the account either. It is forwarded to your wallet in
the same transaction that mints it, so a holder snapshot sees your real address.

## Who can do what

| | |
|---|---|
| You | mint through it, sweep it, rescue anything in it |
| Nock | ask it to mint, with the token going to you |
| Anyone | call `sweep()`, which pays you |
| Nobody | change the owner, withdraw elsewhere, or upgrade it |

## Paid stages

The mint price comes out of this account's balance, which you funded. Nock pays
the gas. Nock never holds the money and never fronts it.
