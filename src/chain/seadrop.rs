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
}
