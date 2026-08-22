use alloy_primitives::Address;
use serde_json::json;
use thiserror::Error;

use super::rpc::{Rpc, RpcError};

/// The `SeaDrop` singleton, the same address on every chain it is deployed to.
pub const SEADROP: &str = "0x00005EA00Ac477B1030CE78506496e8C2dE24bf5";

/// `mintPublic(address,address,address,uint256)`, verified on chain.
const MINT_PUBLIC: [u8; 4] = [0x16, 0x1a, 0xc2, 0x1f];
/// `getPublicDrop(address)`
const GET_PUBLIC_DROP: [u8; 4] = [0xbc, 0x6a, 0x62, 0x9c];
/// `getAllowedFeeRecipients(address)`
const GET_FEE_RECIPIENTS: [u8; 4] = [0x68, 0x63, 0x22, 0x74];

/// The third argument of `mintPublic` sits at 4 selector bytes plus two words.
/// Left zero when an account mints for itself, which is what this tool does:
/// `SeaDrop` refuses a payer that is not the minter unless the collection has
/// allowed it, and a self-hosted wallet is always its own minter.
// Asserted by a test rather than used at runtime: the calldata builder writes
// that word directly, and the constant is what pins the claim that it is where
// we think it is.
#[allow(dead_code)]
pub const MINTER_OFFSET: usize = 68;

#[derive(Debug, Error)]
pub enum SeaDropError {
    #[error("{0}")]
    Rpc(#[from] RpcError),
    #[error("no public stage is configured for this collection")]
    NoPublicDrop,
    #[error("this collection has no allowed fee recipient, so a mint would revert")]
    NoFeeRecipient,
    #[error("the response could not be decoded")]
    Malformed,
    #[allow(dead_code)]
    #[error("expected the {expected} selector and got {got}")]
    WrongSelector { expected: String, got: String },
    #[error("{len} bytes is too short to be a {call}")]
    Truncated { len: usize, call: &'static str },
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct PublicDrop {
    pub mint_price_wei: u128,
    pub start_time: u64,
    pub end_time: u64,
    pub max_per_wallet: u16,
    pub fee_bps: u16,
    pub restrict_fee_recipients: bool,
}

impl PublicDrop {
    /// Used by `watch`, which lands next.
    #[allow(dead_code)]
    #[must_use]
    pub const fn is_open_at(&self, unix_seconds: u64) -> bool {
        unix_seconds >= self.start_time && unix_seconds < self.end_time
    }
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_free(&self) -> bool {
        self.mint_price_wei == 0
    }
}

fn address_arg(address: Address) -> String {
    hex::encode([&[0u8; 12][..], address.as_slice()].concat())
}

/// Reads the stage from the chain rather than from an API.
///
/// This is the difference that matters against the alternative approach of
/// asking `OpenSea` for calldata at T-0: everything needed to build the mint is on
/// chain and readable in advance, so the moment the stage opens there is nothing
/// left to fetch.
pub async fn public_drop(rpc: &mut Rpc, collection: Address) -> Result<PublicDrop, SeaDropError> {
    let data = format!(
        "0x{}{}",
        hex::encode(GET_PUBLIC_DROP),
        address_arg(collection)
    );
    let raw: String = rpc
        .call(
            "eth_call",
            json!([{ "to": SEADROP, "data": data }, "latest"]),
        )
        .await?;
    let bytes = hex::decode(raw.trim_start_matches("0x")).map_err(|_| SeaDropError::Malformed)?;
    if bytes.len() < 192 {
        return Err(SeaDropError::Malformed);
    }

    let word = |i: usize| -> &[u8] { &bytes[i * 32..(i + 1) * 32] };
    let drop = PublicDrop {
        mint_price_wei: be_u128(word(0)),
        start_time: be_u64(word(1)),
        end_time: be_u64(word(2)),
        max_per_wallet: u16::try_from(be_u64(word(3))).unwrap_or(u16::MAX),
        fee_bps: u16::try_from(be_u64(word(4))).unwrap_or(0),
        restrict_fee_recipients: word(5).iter().any(|b| *b != 0),
    };

    // A stage that has never been configured reads as all zeros, which is not a
    // free mint that opened at the epoch.
    if drop.start_time == 0 && drop.end_time == 0 && drop.max_per_wallet == 0 {
        return Err(SeaDropError::NoPublicDrop);
    }
    Ok(drop)
}

/// The fee recipient the collection allows. `mintPublic` reverts on one it does
/// not, so firing without this would waste the slot.
pub async fn fee_recipient(rpc: &mut Rpc, collection: Address) -> Result<Address, SeaDropError> {
    let data = format!(
        "0x{}{}",
        hex::encode(GET_FEE_RECIPIENTS),
        address_arg(collection)
    );
    let raw: String = rpc
        .call(
            "eth_call",
            json!([{ "to": SEADROP, "data": data }, "latest"]),
        )
        .await?;
    let bytes = hex::decode(raw.trim_start_matches("0x")).map_err(|_| SeaDropError::Malformed)?;
    // A dynamic array: an offset word, a length word, then the entries.
    if bytes.len() < 64 {
        return Err(SeaDropError::NoFeeRecipient);
    }
    let count = be_u64(&bytes[32..64]);
    if count == 0 || bytes.len() < 96 {
        return Err(SeaDropError::NoFeeRecipient);
    }
    Ok(Address::from_slice(&bytes[76..96]))
}

/// `mintPublic` calldata for an account minting on its own behalf.
///
/// 132 bytes: four selector bytes and four words. `OpenSea`'s own API appends four
/// attribution bytes to theirs; we append none, which is the only difference
/// between the two.
#[must_use]
pub fn mint_public_calldata(collection: Address, fee_to: Address, quantity: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(132);
    out.extend_from_slice(&MINT_PUBLIC);
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(collection.as_slice());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(fee_to.as_slice());
    // minterIfNotPayer, left zero so SeaDrop treats msg.sender as the minter.
    out.extend_from_slice(&[0u8; 32]);
    let mut qty = [0u8; 32];
    qty[24..32].copy_from_slice(&quantity.to_be_bytes());
    out.extend_from_slice(&qty);
    out
}

fn be_u64(word: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let start = word.len().saturating_sub(8);
    buf.copy_from_slice(&word[start..]);
    u64::from_be_bytes(buf)
}

fn be_u128(word: &[u8]) -> u128 {
    let mut buf = [0u8; 16];
    let start = word.len().saturating_sub(16);
    buf.copy_from_slice(&word[start..]);
    u128::from_be_bytes(buf)
}

/// `mintSigned(address,address,address,uint256,MintParams,uint256,bytes)`,
/// confirmed against the `OpenSea` mint route on chain 4663.
#[allow(dead_code)]
pub const MINT_SIGNED: [u8; 4] = [0x4b, 0x61, 0xcd, 0x6f];
/// `getSignedMintValidationParams(address,address)`, confirmed 2026-08-19.
const GET_VALIDATION_PARAMS: [u8; 4] = [0x81, 0xbf, 0x9a, 0xf3];
/// `getSigners(address)`, confirmed 2026-08-19.
const GET_SIGNERS: [u8; 4] = [0x7e, 0x3b, 0xa6, 0xaf];

/// `totalSupply()` and `maxSupply()` on the collection itself.
const TOTAL_SUPPLY: [u8; 4] = [0x18, 0x16, 0x0d, 0xdd];
const MAX_SUPPLY: [u8; 4] = [0xd5, 0xab, 0xeb, 0x01];

/// How much of a collection is left.
///
/// Read before anything is signed, because `SeaDrop` reverts a mint past the
/// supply with `MintQuantityExceedsMaxSupply` and a revert costs the gas of a
/// transaction that was never going to work. Measured 2026-08-19: a live stage
/// that looked open was already 888 of 888 gone, and the only way to find out
/// was to pay for it.
///
/// `None` when either getter is missing, which some collections do not
/// implement. Missing evidence is not a reason to refuse a mint.
#[allow(dead_code)]
pub async fn supply_left(rpc: &mut Rpc, collection: Address) -> Option<u64> {
    let read = |data: [u8; 4]| format!("0x{}", hex::encode(data));
    let total: String = rpc
        .call(
            "eth_call",
            json!([{ "to": format!("{collection:?}"), "data": read(TOTAL_SUPPLY) }, "latest"]),
        )
        .await
        .ok()?;
    let max: String = rpc
        .call(
            "eth_call",
            json!([{ "to": format!("{collection:?}"), "data": read(MAX_SUPPLY) }, "latest"]),
        )
        .await
        .ok()?;

    let word = |raw: &str| -> Option<u64> {
        let bytes = hex::decode(raw.trim_start_matches("0x")).ok()?;
        Some(be_u64(word_at(&bytes, 0)?))
    };
    let (total, max) = (word(&total)?, word(&max)?);
    Some(max.saturating_sub(total))
}

/// A `mintPublic` call, read back out of its calldata.
///
/// Four words and no struct, which is the whole difference from a signed mint:
/// a public stage has nothing to authorise, so there is nothing to sign and
/// nothing to verify a signature against.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct PublicMintCall {
    pub nft_contract: Address,
    pub fee_recipient: Address,
    /// Zero when the payer mints for itself. `OpenSea` fills this in explicitly
    /// with the connected wallet; our own builder leaves it zero. Both mint to
    /// the same address, so both are accepted and checked against the wallet.
    pub minter: Address,
    pub quantity: u64,
}

/// Reads a `mintPublic` call apart so every field can be checked.
///
/// Tolerates trailing bytes. `OpenSea` appends four of them, `3d958fe2`,
/// measured on a real response 2026-08-19: an attribution tag that rides along
/// after the last ABI word. Refusing calldata for having it would refuse every
/// mint they build.
#[allow(dead_code)]
pub fn decode_mint_public(data: &[u8]) -> Result<PublicMintCall, SeaDropError> {
    let short = || SeaDropError::Truncated {
        len: data.len(),
        call: "mintPublic",
    };
    let selector = data.get(0..4).ok_or_else(short)?;
    if selector != MINT_PUBLIC {
        return Err(SeaDropError::WrongSelector {
            expected: hex::encode(MINT_PUBLIC),
            got: hex::encode(selector),
        });
    }
    let body = &data[4..];
    let w = |i: usize| word_at(body, i).ok_or_else(short);
    let address =
        |i: usize| -> Result<Address, SeaDropError> { Ok(Address::from_slice(&w(i)?[12..32])) };

    Ok(PublicMintCall {
        nft_contract: address(0)?,
        fee_recipient: address(1)?,
        minter: address(2)?,
        quantity: be_u64(w(3)?),
    })
}

/// The bounds a collection published for what its signer may sign within.
///
/// This is the independent anchor under the whole signed-stage path. `OpenSea` is
/// the only source of the signature itself and cannot be checked directly, but
/// the terms that signature carries can be checked against what the collection
/// itself put on chain. A compromised or buggy response cannot make us sign a
/// price or a quantity the collection never authorised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationParams {
    pub min_mint_price_wei: u128,
    pub max_total_mintable_by_wallet: u32,
    pub min_start_time: u64,
    pub max_end_time: u64,
    pub max_token_supply_for_stage: u64,
    pub min_fee_bps: u16,
    pub max_fee_bps: u16,
}

/// Everything a `mintSigned` call is asking for, read back out of its calldata.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SignedMintCall {
    pub nft_contract: Address,
    pub fee_recipient: Address,
    pub minter: Address,
    pub quantity: u64,
    pub mint_price_wei: u128,
    pub max_total_mintable_by_wallet: u64,
    pub start_time: u64,
    pub end_time: u64,
    pub drop_stage_index: u64,
    pub max_token_supply_for_stage: u64,
    pub fee_bps: u16,
    pub restrict_fee_recipients: bool,
    pub salt: [u8; 32],
    pub signature: Vec<u8>,
}

fn word_at(data: &[u8], index: usize) -> Option<&[u8]> {
    data.get(index * 32..index * 32 + 32)
}

pub fn decode_validation_params(raw: &[u8]) -> Result<ValidationParams, SeaDropError> {
    let w = |i: usize| {
        word_at(raw, i).ok_or(SeaDropError::Truncated {
            len: raw.len(),
            call: "SignedMintValidationParams",
        })
    };
    Ok(ValidationParams {
        min_mint_price_wei: be_u128(w(0)?),
        max_total_mintable_by_wallet: u32::try_from(be_u64(w(1)?)).unwrap_or(u32::MAX),
        min_start_time: be_u64(w(2)?),
        max_end_time: be_u64(w(3)?),
        max_token_supply_for_stage: be_u64(w(4)?),
        min_fee_bps: u16::try_from(be_u64(w(5)?)).unwrap_or(u16::MAX),
        max_fee_bps: u16::try_from(be_u64(w(6)?)).unwrap_or(u16::MAX),
    })
}

pub fn decode_signers(raw: &[u8]) -> Result<Vec<Address>, SeaDropError> {
    let short = || SeaDropError::Truncated {
        len: raw.len(),
        call: "getSigners",
    };
    // Word 0 is the offset to the array, word 1 its length, then the elements.
    let count = usize::try_from(be_u64(word_at(raw, 1).ok_or_else(short)?)).unwrap_or(0);
    (0..count)
        .map(|i| {
            let w = word_at(raw, 2 + i).ok_or_else(short)?;
            Ok(Address::from_slice(&w[12..32]))
        })
        .collect()
}

/// Reads a `mintSigned` call apart so every field can be checked.
///
/// `MintParams` is a static struct of eight words, so it is inlined in the head
/// rather than pointed at. Only `signature` is dynamic, and its offset word
/// points past the head to a length followed by the bytes.
#[allow(dead_code)]
pub fn decode_mint_signed(data: &[u8]) -> Result<SignedMintCall, SeaDropError> {
    let short = || SeaDropError::Truncated {
        len: data.len(),
        call: "mintSigned",
    };
    let selector = data.get(0..4).ok_or_else(short)?;
    if selector != MINT_SIGNED {
        return Err(SeaDropError::WrongSelector {
            expected: hex::encode(MINT_SIGNED),
            got: hex::encode(selector),
        });
    }
    let body = &data[4..];
    let w = |i: usize| word_at(body, i).ok_or_else(short);
    let address =
        |i: usize| -> Result<Address, SeaDropError> { Ok(Address::from_slice(&w(i)?[12..32])) };

    let sig_offset = usize::try_from(be_u64(w(13)?)).map_err(|_| short())?;
    let sig_len_index = sig_offset.checked_div(32).ok_or_else(short)?;
    let sig_len = usize::try_from(be_u64(w(sig_len_index)?)).map_err(|_| short())?;
    let sig_start = sig_offset + 32;
    let signature = body
        .get(sig_start..sig_start + sig_len)
        .ok_or_else(short)?
        .to_vec();

    let mut salt = [0u8; 32];
    salt.copy_from_slice(w(12)?);

    Ok(SignedMintCall {
        nft_contract: address(0)?,
        fee_recipient: address(1)?,
        minter: address(2)?,
        quantity: be_u64(w(3)?),
        mint_price_wei: be_u128(w(4)?),
        max_total_mintable_by_wallet: be_u64(w(5)?),
        start_time: be_u64(w(6)?),
        end_time: be_u64(w(7)?),
        drop_stage_index: be_u64(w(8)?),
        max_token_supply_for_stage: be_u64(w(9)?),
        fee_bps: u16::try_from(be_u64(w(10)?)).unwrap_or(u16::MAX),
        restrict_fee_recipients: w(11)?.iter().any(|b| *b != 0),
        salt,
        signature,
    })
}

/// The bounds this collection published for this signer.
#[allow(dead_code)]
pub async fn validation_params(
    rpc: &mut Rpc,
    collection: Address,
    signer: Address,
) -> Result<ValidationParams, SeaDropError> {
    let data = format!(
        "0x{}{}{}",
        hex::encode(GET_VALIDATION_PARAMS),
        address_arg(collection),
        address_arg(signer)
    );
    let raw: String = rpc
        .call(
            "eth_call",
            json!([{ "to": SEADROP, "data": data }, "latest"]),
        )
        .await?;
    let bytes = hex::decode(raw.trim_start_matches("0x")).map_err(|_| SeaDropError::Malformed)?;
    decode_validation_params(&bytes)
}

/// Who this collection allows to sign for it, read from chain rather than from
/// our own index of `SignerUpdated` events.
#[allow(dead_code)]
pub async fn signers(rpc: &mut Rpc, collection: Address) -> Result<Vec<Address>, SeaDropError> {
    let data = format!("0x{}{}", hex::encode(GET_SIGNERS), address_arg(collection));
    let raw: String = rpc
        .call(
            "eth_call",
            json!([{ "to": SEADROP, "data": data }, "latest"]),
        )
        .await?;
    let bytes = hex::decode(raw.trim_start_matches("0x")).map_err(|_| SeaDropError::Malformed)?;
    decode_signers(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &str = "0x20D7caDa668d1Fd24dDf905cB79069934A77309d";
    const FEE: &str = "0x0000a26b00c1F0DF003000390027140000fAa719";

    #[test]
    fn the_calldata_is_the_shape_seadrop_accepts() {
        let data = mint_public_calldata(COLLECTION.parse().unwrap(), FEE.parse().unwrap(), 1);
        // 4 + 4 * 32. Anything else means the offset arithmetic is wrong.
        assert_eq!(data.len(), 132);
        assert_eq!(&data[..4], &MINT_PUBLIC);
    }

    /// The whole design rests on this word being where we think it is.
    #[test]
    fn the_minter_slot_is_zero_and_sits_at_offset_68() {
        let data = mint_public_calldata(COLLECTION.parse().unwrap(), FEE.parse().unwrap(), 1);
        assert_eq!(&data[MINTER_OFFSET..MINTER_OFFSET + 32], &[0u8; 32]);
    }

    #[test]
    fn the_arguments_land_in_the_right_words() {
        let collection: Address = COLLECTION.parse().unwrap();
        let fee: Address = FEE.parse().unwrap();
        let data = mint_public_calldata(collection, fee, 3);
        assert_eq!(&data[16..36], collection.as_slice());
        assert_eq!(&data[48..68], fee.as_slice());
        assert_eq!(be_u64(&data[100..132]), 3);
    }

    /// Matches the vector `scripts/sign-vector.ts` signs, which is itself the
    /// calldata `OpenSea`'s API produces minus its four attribution bytes.
    #[test]
    fn it_matches_the_calldata_in_the_signing_vector() {
        let data = mint_public_calldata(COLLECTION.parse().unwrap(), FEE.parse().unwrap(), 1);
        assert_eq!(
            hex::encode(&data),
            "161ac21f\
             00000000000000000000000020d7cada668d1fd24ddf905cb79069934a77309d\
             0000000000000000000000000000a26b00c1f0df003000390027140000faa719\
             0000000000000000000000000000000000000000000000000000000000000000\
             0000000000000000000000000000000000000000000000000000000000000001"
                .replace([' ', '\n'], "")
        );
    }

    #[test]
    fn a_stage_knows_whether_it_is_open() {
        let drop = PublicDrop {
            mint_price_wei: 0,
            start_time: 1_000,
            end_time: 2_000,
            max_per_wallet: 3,
            fee_bps: 500,
            restrict_fee_recipients: true,
        };
        assert!(!drop.is_open_at(999));
        assert!(drop.is_open_at(1_000));
        assert!(drop.is_open_at(1_999));
        // The end is exclusive: a stage that ended is closed, not open.
        assert!(!drop.is_open_at(2_000));
        assert!(drop.is_free());
    }

    #[test]
    fn reads_big_endian_words_from_the_right_end() {
        let mut word = [0u8; 32];
        word[31] = 0x2a;
        assert_eq!(be_u64(&word), 42);
        assert_eq!(be_u128(&word), 42);
    }

    // Captured from chain 4663 on 2026-08-19 with eth_call against the SeaDrop
    // singleton, collection 0x941c2a17c60ad6daf86cb6438074d57e906adffa and
    // signer 0xfce4b31128100915f2980bbc3a08894ee5e8f8c3. Real bytes rather than
    // bytes we constructed, so this pins the shape the chain actually returns.
    const VALIDATION_PARAMS: &str = "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003a98000000000000000000000000000000000000000000000000000000006a82f305000000000000000000000000000000000000000000000000000000006a86eb090000000000000000000000000000000000000000000000000000000000003a9800000000000000000000000000000000000000000000000000000000000003e800000000000000000000000000000000000000000000000000000000000003e8";

    const SIGNERS_RETURN: &str = "00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000fce4b31128100915f2980bbc3a08894ee5e8f8c3";

    #[test]
    fn it_decodes_the_bounds_the_collection_published() {
        let raw = hex::decode(VALIDATION_PARAMS).unwrap();
        let p = decode_validation_params(&raw).unwrap();
        assert_eq!(p.min_mint_price_wei, 0);
        assert_eq!(p.max_total_mintable_by_wallet, 15_000);
        assert_eq!(p.min_start_time, 1_786_966_789);
        assert_eq!(p.max_end_time, 1_787_226_889);
        assert_eq!(p.max_token_supply_for_stage, 15_000);
        assert_eq!(p.min_fee_bps, 1_000);
        assert_eq!(p.max_fee_bps, 1_000);
    }

    // getSigners is what lets us confirm the authorised signer from chain rather
    // than trusting our own index of SignerUpdated events.
    #[test]
    fn it_decodes_the_signer_list() {
        let raw = hex::decode(SIGNERS_RETURN).unwrap();
        let list = decode_signers(&raw).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            format!("{:?}", list[0]).to_lowercase(),
            "0xfce4b31128100915f2980bbc3a08894ee5e8f8c3"
        );
    }

    #[test]
    fn it_decodes_an_empty_signer_list_rather_than_failing() {
        let raw = hex::decode("00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000").unwrap();
        assert!(decode_signers(&raw).unwrap().is_empty());
    }

    // Anything that is not mintSigned must be refused before it reaches a
    // signer. This is the first gate of the verification table.
    #[test]
    fn it_refuses_calldata_that_is_not_mint_signed() {
        let data = mint_public_calldata(
            "0x941c2a17c60ad6daf86cb6438074d57e906adffa"
                .parse()
                .unwrap(),
            "0x0000a26b00c1F0DF003000390027140000fAa719"
                .parse()
                .unwrap(),
            1,
        );
        assert!(matches!(
            decode_mint_signed(&data),
            Err(SeaDropError::WrongSelector { .. })
        ));
    }

    // The exact calldata OpenSea returned for sushicatart on 2026-08-19, four
    // attribution bytes and all. Real bytes, not bytes we built.
    const REAL_MINT_PUBLIC: &str = "161ac21f000000000000000000000000941c2a17c60ad6daf86cb6438074d57e906adffa0000000000000000000000000000a26b00c1f0df003000390027140000faa7190000000000000000000000007bd7ec70346f762b8a6296b45eaec65af874aa4b00000000000000000000000000000000000000000000000000000000000000013d958fe2";

    #[test]
    fn it_reads_a_real_mint_public_call_from_opensea() {
        let data = hex::decode(REAL_MINT_PUBLIC).unwrap();
        let call = decode_mint_public(&data).unwrap();
        assert_eq!(
            format!("{:?}", call.nft_contract).to_lowercase(),
            "0x941c2a17c60ad6daf86cb6438074d57e906adffa"
        );
        assert_eq!(
            format!("{:?}", call.minter).to_lowercase(),
            "0x7bd7ec70346f762b8a6296b45eaec65af874aa4b"
        );
        assert_eq!(call.quantity, 1);
    }

    // Four bytes ride along after the last word. Refusing them would refuse
    // every mint OpenSea builds.
    #[test]
    fn it_tolerates_the_attribution_bytes_opensea_appends() {
        let data = hex::decode(REAL_MINT_PUBLIC).unwrap();
        assert_eq!(
            data.len(),
            4 + 4 * 32 + 4,
            "selector, four words, four extra"
        );
        assert!(decode_mint_public(&data).is_ok());
    }

    #[test]
    fn it_refuses_a_public_call_that_is_really_a_signed_one() {
        let data = hex::decode(REAL_MINT_PUBLIC).unwrap();
        let mut signed = data.clone();
        signed[0..4].copy_from_slice(&MINT_SIGNED);
        assert!(matches!(
            decode_mint_public(&signed),
            Err(SeaDropError::WrongSelector { .. })
        ));
    }

    #[test]
    fn it_refuses_calldata_too_short_to_hold_the_call() {
        assert!(decode_mint_signed(&[0x4b, 0x61, 0xcd, 0x6f]).is_err());
        assert!(decode_mint_signed(&[]).is_err());
    }

    #[test]
    fn it_reads_every_field_of_a_signed_mint_call() {
        let data = sample_mint_signed();
        let call = decode_mint_signed(&data).unwrap();
        assert_eq!(
            format!("{:?}", call.nft_contract).to_lowercase(),
            "0x941c2a17c60ad6daf86cb6438074d57e906adffa"
        );
        assert_eq!(
            format!("{:?}", call.minter).to_lowercase(),
            "0x00000000000000000000000000000000000000aa"
        );
        assert_eq!(call.quantity, 2);
        assert_eq!(call.mint_price_wei, 15_000_000_000_000);
        assert_eq!(call.max_total_mintable_by_wallet, 5);
        assert_eq!(call.start_time, 1_786_966_789);
        assert_eq!(call.end_time, 1_787_226_889);
        assert_eq!(call.drop_stage_index, 1);
        assert_eq!(call.fee_bps, 1_000);
        assert!(call.restrict_fee_recipients);
        assert_eq!(call.signature.len(), 65);
        assert_eq!(call.signature[64], 27);
    }

    /// A mintSigned call built word by word, so the decoder is read against a
    /// layout written out by hand rather than against itself.
    fn sample_mint_signed() -> Vec<u8> {
        let word = |v: u128| {
            let mut w = [0u8; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            w
        };
        let addr = |hexs: &str| {
            let mut w = [0u8; 32];
            w[12..].copy_from_slice(&hex::decode(hexs).unwrap());
            w
        };
        let mut d = Vec::new();
        d.extend_from_slice(&[0x4b, 0x61, 0xcd, 0x6f]);
        d.extend_from_slice(&addr("941c2a17c60ad6daf86cb6438074d57e906adffa")); // 0 nft
        d.extend_from_slice(&addr("0000a26b00c1f0df003000390027140000faa719")); // 1 fee
        d.extend_from_slice(&addr("00000000000000000000000000000000000000aa")); // 2 minter
        d.extend_from_slice(&word(2)); // 3 quantity
        d.extend_from_slice(&word(15_000_000_000_000)); // 4 mintPrice
        d.extend_from_slice(&word(5)); // 5 maxTotalMintableByWallet
        d.extend_from_slice(&word(1_786_966_789)); // 6 startTime
        d.extend_from_slice(&word(1_787_226_889)); // 7 endTime
        d.extend_from_slice(&word(1)); // 8 dropStageIndex
        d.extend_from_slice(&word(15_000)); // 9 maxTokenSupplyForStage
        d.extend_from_slice(&word(1_000)); // 10 feeBps
        d.extend_from_slice(&word(1)); // 11 restrictFeeRecipients
        d.extend_from_slice(&word(13 * 32)); // 12 salt
        d.extend_from_slice(&word(14 * 32)); // 13 offset to signature
        d.extend_from_slice(&word(65)); // 14 signature length
        let mut sig = [0u8; 65];
        sig[64] = 27;
        d.extend_from_slice(&sig);
        d.extend_from_slice(&[0u8; 31]); // pad the tail to a whole word
        d
    }

    // Not run by default: it needs the network, and a test that fails because an
    // endpoint was busy teaches people to ignore red. Run it on purpose with
    //   cargo test -- --ignored reads_the_live_bounds
    // whenever the SeaDrop deployment or the chain changes under us.
    #[tokio::test]
    #[ignore = "requires the live Robinhood Chain RPC"]
    async fn reads_the_live_bounds_and_signer_from_chain_4663() {
        use crate::chain::rpc::Rpc;
        use std::time::Duration;

        let mut rpc = Rpc::new(
            vec!["https://rpc.mainnet.chain.robinhood.com".to_owned()],
            Duration::from_secs(15),
        );
        let collection: Address = "0x941c2a17c60ad6daf86cb6438074d57e906adffa"
            .parse()
            .unwrap();

        let found = signers(&mut rpc, collection).await.unwrap();
        assert_eq!(found.len(), 1, "expected exactly one authorised signer");

        let bounds = validation_params(&mut rpc, collection, found[0])
            .await
            .unwrap();
        assert_eq!(bounds.max_total_mintable_by_wallet, 15_000);
        assert_eq!(bounds.min_fee_bps, 1_000);
        assert_eq!(bounds.max_fee_bps, 1_000);
    }
}
