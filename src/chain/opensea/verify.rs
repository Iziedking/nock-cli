use alloy_primitives::Address;

use crate::chain::seadrop::{
    decode_mint_signed, SeaDropError, SignedMintCall, ValidationParams, SEADROP,
};

/// What makes an untrusted supplier acceptable on a money path.
///
/// `OpenSea` holds the collection's signer key, so the 65-byte signature is the
/// one thing here that cannot be computed, derived or read off chain. Everything
/// wrapped around it can be, and is. Nothing reaches the signer until every
/// check below has passed against a value we already knew from somewhere else:
/// the address the user typed, the wallet they unlocked, the fee recipients the
/// collection published, and the bounds the collection published for its signer.
///
/// The point is not that `OpenSea` is expected to misbehave. It is that a change
/// on their side, a bug on ours, or a compromise anywhere between should cost a
/// refused wallet and a printed reason, never a signed transaction that does
/// something other than what was asked.
///
/// On failure this names the field and carries both values, because "refused" on
/// its own sends somebody to read our source at T-2 minutes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionData {
    pub to: Address,
    pub data: Vec<u8>,
    pub value_wei: u128,
}

/// Everything we knew before asking, to check the answer against.
#[derive(Debug, Clone)]
pub struct Expectation {
    pub collection: Address,
    /// The wallet that unlocked in this run, and the only address a token from
    /// this call may go to.
    pub minter: Address,
    pub quantity: u64,
    pub unit_price_wei: u128,
    /// Read from `getAllowedFeeRecipients` on chain. Empty means we could not
    /// read them, which is missing evidence rather than permission.
    pub allowed_fee_recipients: Vec<Address>,
    /// The bounds the collection published for the signer this stage uses, from
    /// `getSignedMintValidationParams`. `None` when the getter could not be read;
    /// the rest of the table still stands on its own.
    pub bounds: Option<ValidationParams>,
    /// What is left of `--max-spend` for the whole run.
    pub spend_remaining_wei: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Decode(String),
    To {
        expected: Address,
        got: Address,
    },
    Collection {
        expected: Address,
        got: Address,
    },
    Minter {
        expected: Address,
        got: Address,
    },
    Quantity {
        expected: u64,
        got: u64,
    },
    UnitPrice {
        expected_wei: u128,
        got_wei: u128,
    },
    Value {
        expected_wei: u128,
        got_wei: u128,
    },
    FeeRecipient {
        got: Address,
        allowed: Vec<Address>,
    },
    PriceBelowFloor {
        floor_wei: u128,
        got_wei: u128,
    },
    QuantityAboveCap {
        cap: u64,
        got: u64,
    },
    WindowOutsideBounds {
        allowed: (u64, u64),
        got: (u64, u64),
    },
    FeeBpsOutsideBounds {
        allowed: (u16, u16),
        got: u16,
    },
    OverSpendCeiling {
        needed_wei: u128,
        remaining_wei: u128,
    },
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(why) => write!(f, "the calldata could not be read: {why}"),
            Self::To { expected, got } => write!(
                f,
                "the call goes to {got:?}, not to the SeaDrop singleton {expected:?}"
            ),
            Self::Collection { expected, got } => write!(
                f,
                "the calldata mints {got:?}, not the collection asked for, {expected:?}"
            ),
            Self::Minter { expected, got } => write!(
                f,
                "the token would go to {got:?}, not to this wallet {expected:?}"
            ),
            Self::Quantity { expected, got } => {
                write!(f, "the calldata asks for {got}, not the {expected} requested")
            }
            Self::UnitPrice { expected_wei, got_wei } => write!(
                f,
                "the calldata prices this at {got_wei} wei each, we were quoted {expected_wei}"
            ),
            Self::Value { expected_wei, got_wei } => write!(
                f,
                "the call would send {got_wei} wei, and price times quantity is {expected_wei}"
            ),
            Self::FeeRecipient { got, allowed } => write!(
                f,
                "{got:?} is not an allowed fee recipient, so the mint would revert. Allowed: {allowed:?}"
            ),
            Self::PriceBelowFloor { floor_wei, got_wei } => write!(
                f,
                "the price {got_wei} wei is below the {floor_wei} wei floor the collection published"
            ),
            Self::QuantityAboveCap { cap, got } => write!(
                f,
                "{got} is above the {cap} per wallet the collection published"
            ),
            Self::WindowOutsideBounds { allowed, got } => write!(
                f,
                "the stage window {got:?} is outside the {allowed:?} the collection published"
            ),
            Self::FeeBpsOutsideBounds { allowed, got } => write!(
                f,
                "a fee of {got} bps is outside the {allowed:?} the collection published"
            ),
            Self::OverSpendCeiling { needed_wei, remaining_wei } => write!(
                f,
                "this needs {needed_wei} wei and only {remaining_wei} wei of --max-spend is left"
            ),
        }
    }
}

/// Checks a submission against everything already known, and returns the decoded
/// call only if all of it holds.
///
/// Order matters for the message, not the verdict: the earliest check that fails
/// is the one reported, and the earliest checks are the ones whose failure says
/// the most. A call to the wrong contract is a different kind of wrong from a
/// price that drifted.
pub fn verify(
    submission: &SubmissionData,
    expect: &Expectation,
) -> Result<SignedMintCall, Rejection> {
    let seadrop: Address = SEADROP.parse().expect("the SeaDrop constant is an address");
    if submission.to != seadrop {
        return Err(Rejection::To {
            expected: seadrop,
            got: submission.to,
        });
    }

    // Refuses anything that is not a mintSigned selector, before any field of it
    // is believed.
    let call = decode_mint_signed(&submission.data)
        .map_err(|e: SeaDropError| Rejection::Decode(e.to_string()))?;

    if call.nft_contract != expect.collection {
        return Err(Rejection::Collection {
            expected: expect.collection,
            got: call.nft_contract,
        });
    }
    // The field that decides who ends up owning the token. Everything else here
    // is money; this one is the whole purpose of the tool.
    if call.minter != expect.minter {
        return Err(Rejection::Minter {
            expected: expect.minter,
            got: call.minter,
        });
    }
    if call.quantity != expect.quantity {
        return Err(Rejection::Quantity {
            expected: expect.quantity,
            got: call.quantity,
        });
    }
    if call.mint_price_wei != expect.unit_price_wei {
        return Err(Rejection::UnitPrice {
            expected_wei: expect.unit_price_wei,
            got_wei: call.mint_price_wei,
        });
    }

    let total_wei = expect
        .unit_price_wei
        .saturating_mul(u128::from(expect.quantity));
    if submission.value_wei != total_wei {
        return Err(Rejection::Value {
            expected_wei: total_wei,
            got_wei: submission.value_wei,
        });
    }

    // An empty allow list is missing evidence, not permission: we simply could
    // not read them, and condemning a fine drop on that would be worse than the
    // check is worth. A non-empty list that excludes this recipient is a mint
    // that reverts, which is a wasted slot rather than a loss, and free to catch.
    if !expect.allowed_fee_recipients.is_empty()
        && !expect.allowed_fee_recipients.contains(&call.fee_recipient)
    {
        return Err(Rejection::FeeRecipient {
            got: call.fee_recipient,
            allowed: expect.allowed_fee_recipients.clone(),
        });
    }

    // The on-chain anchor. These are the terms the collection itself published
    // for what its signer may sign within, so a response asking for anything
    // outside them was not authorised by the collection whatever it claims.
    if let Some(bounds) = expect.bounds {
        if call.mint_price_wei < bounds.min_mint_price_wei {
            return Err(Rejection::PriceBelowFloor {
                floor_wei: bounds.min_mint_price_wei,
                got_wei: call.mint_price_wei,
            });
        }
        if call.quantity > u64::from(bounds.max_total_mintable_by_wallet) {
            return Err(Rejection::QuantityAboveCap {
                cap: u64::from(bounds.max_total_mintable_by_wallet),
                got: call.quantity,
            });
        }
        if call.start_time < bounds.min_start_time || call.end_time > bounds.max_end_time {
            return Err(Rejection::WindowOutsideBounds {
                allowed: (bounds.min_start_time, bounds.max_end_time),
                got: (call.start_time, call.end_time),
            });
        }
        if call.fee_bps < bounds.min_fee_bps || call.fee_bps > bounds.max_fee_bps {
            return Err(Rejection::FeeBpsOutsideBounds {
                allowed: (bounds.min_fee_bps, bounds.max_fee_bps),
                got: call.fee_bps,
            });
        }
    }

    // Last, because it is about the run rather than about this call being
    // honest. A correct call we cannot afford is still refused.
    if total_wei > expect.spend_remaining_wei {
        return Err(Rejection::OverSpendCeiling {
            needed_wei: total_wei,
            remaining_wei: expect.spend_remaining_wei,
        });
    }

    Ok(call)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTION: &str = "0x941c2a17c60ad6daf86cb6438074d57e906adffa";
    const FEE: &str = "0x0000a26b00c1f0df003000390027140000faa719";
    const MINTER: &str = "0x00000000000000000000000000000000000000aa";
    const PRICE: u128 = 15_000_000_000_000;
    const QUANTITY: u64 = 2;

    /// Builds a `mintSigned` call word by word, so the tests are written against
    /// the ABI layout rather than against our own decoder.
    struct Call {
        nft: String,
        fee: String,
        minter: String,
        quantity: u64,
        price: u128,
        cap: u64,
        start: u64,
        end: u64,
        fee_bps: u64,
    }

    impl Default for Call {
        fn default() -> Self {
            Self {
                nft: COLLECTION.to_owned(),
                fee: FEE.to_owned(),
                minter: MINTER.to_owned(),
                quantity: QUANTITY,
                price: PRICE,
                cap: 5,
                start: 1_786_966_789,
                end: 1_787_226_889,
                fee_bps: 1_000,
            }
        }
    }

    impl Call {
        fn encode(&self) -> Vec<u8> {
            let word = |v: u128| {
                let mut w = [0u8; 32];
                w[16..].copy_from_slice(&v.to_be_bytes());
                w
            };
            let addr = |a: &str| {
                let mut w = [0u8; 32];
                w[12..].copy_from_slice(&hex::decode(a.trim_start_matches("0x")).unwrap());
                w
            };
            let mut d = vec![0x4b, 0x61, 0xcd, 0x6f];
            d.extend_from_slice(&addr(&self.nft));
            d.extend_from_slice(&addr(&self.fee));
            d.extend_from_slice(&addr(&self.minter));
            d.extend_from_slice(&word(u128::from(self.quantity)));
            d.extend_from_slice(&word(self.price));
            d.extend_from_slice(&word(u128::from(self.cap)));
            d.extend_from_slice(&word(u128::from(self.start)));
            d.extend_from_slice(&word(u128::from(self.end)));
            d.extend_from_slice(&word(1)); // dropStageIndex
            d.extend_from_slice(&word(15_000)); // maxTokenSupplyForStage
            d.extend_from_slice(&word(u128::from(self.fee_bps)));
            d.extend_from_slice(&word(1)); // restrictFeeRecipients
            d.extend_from_slice(&word(0)); // salt
            d.extend_from_slice(&word(14 * 32)); // offset to the signature
            d.extend_from_slice(&word(65)); // signature length
            d.extend_from_slice(&[0u8; 65]);
            d.extend_from_slice(&[0u8; 31]);
            d
        }
    }

    fn bounds() -> ValidationParams {
        ValidationParams {
            min_mint_price_wei: 0,
            max_total_mintable_by_wallet: 15_000,
            min_start_time: 1_786_966_789,
            max_end_time: 1_787_226_889,
            max_token_supply_for_stage: 15_000,
            min_fee_bps: 1_000,
            max_fee_bps: 1_000,
        }
    }

    fn good() -> (SubmissionData, Expectation) {
        (
            SubmissionData {
                to: SEADROP.parse().unwrap(),
                data: Call::default().encode(),
                value_wei: PRICE * u128::from(QUANTITY),
            },
            Expectation {
                collection: COLLECTION.parse().unwrap(),
                minter: MINTER.parse().unwrap(),
                quantity: QUANTITY,
                unit_price_wei: PRICE,
                allowed_fee_recipients: vec![FEE.parse().unwrap()],
                bounds: Some(bounds()),
                spend_remaining_wei: u128::MAX,
            },
        )
    }

    #[test]
    fn it_accepts_a_submission_where_every_field_matches() {
        let (s, e) = good();
        let call = verify(&s, &e).unwrap();
        assert_eq!(call.quantity, QUANTITY);
        assert_eq!(call.mint_price_wei, PRICE);
    }

    #[test]
    fn it_refuses_a_call_that_is_not_to_the_seadrop_singleton() {
        let (mut s, e) = good();
        s.to = "0x000000000000000000000000000000000000dead"
            .parse()
            .unwrap();
        assert!(matches!(verify(&s, &e), Err(Rejection::To { .. })));
    }

    #[test]
    fn it_refuses_calldata_that_is_not_mint_signed() {
        let (mut s, e) = good();
        s.data[0] = 0xff;
        assert!(matches!(verify(&s, &e), Err(Rejection::Decode(_))));
    }

    #[test]
    fn it_refuses_calldata_for_a_different_collection() {
        let (s, mut e) = good();
        e.collection = "0x000000000000000000000000000000000000beef"
            .parse()
            .unwrap();
        assert!(matches!(verify(&s, &e), Err(Rejection::Collection { .. })));
    }

    // The field that decides who owns the token. If only one check survived,
    // this would be the one worth keeping.
    #[test]
    fn it_refuses_calldata_that_would_mint_to_somebody_else() {
        let (s, mut e) = good();
        e.minter = "0x000000000000000000000000000000000000f00d"
            .parse()
            .unwrap();
        assert!(matches!(verify(&s, &e), Err(Rejection::Minter { .. })));
    }

    #[test]
    fn it_refuses_a_quantity_we_did_not_ask_for() {
        let (s, mut e) = good();
        e.quantity = QUANTITY + 1;
        assert!(matches!(verify(&s, &e), Err(Rejection::Quantity { .. })));
    }

    // A price that moved between the quote and the calldata is the quiet way to
    // be overcharged, because everything else about the call still looks right.
    #[test]
    fn it_refuses_a_unit_price_that_drifted_from_the_quote() {
        let (s, mut e) = good();
        e.unit_price_wei = PRICE * 2;
        assert!(matches!(verify(&s, &e), Err(Rejection::UnitPrice { .. })));
    }

    #[test]
    fn it_refuses_a_value_that_is_not_price_times_quantity() {
        let (mut s, e) = good();
        s.value_wei += 1;
        assert!(matches!(verify(&s, &e), Err(Rejection::Value { .. })));
    }

    // mintPublic and mintSigned both revert on a fee recipient the collection
    // does not allow, so this is a wasted slot caught for the price of a list
    // lookup.
    #[test]
    fn it_refuses_a_fee_recipient_the_collection_does_not_allow() {
        let (s, mut e) = good();
        e.allowed_fee_recipients = vec!["0x000000000000000000000000000000000000aaaa"
            .parse()
            .unwrap()];
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::FeeRecipient { .. })
        ));
    }

    // Not having read the list is not the same as the list being empty, and
    // condemning a fine drop on missing evidence is worse than the check is
    // worth.
    #[test]
    fn it_treats_an_unreadable_fee_recipient_list_as_missing_evidence() {
        let (s, mut e) = good();
        e.allowed_fee_recipients = Vec::new();
        assert!(verify(&s, &e).is_ok());
    }

    #[test]
    fn it_refuses_a_quantity_above_the_cap_the_collection_published() {
        let (s, mut e) = good();
        let mut b = bounds();
        b.max_total_mintable_by_wallet = 1;
        e.bounds = Some(b);
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::QuantityAboveCap { cap: 1, got: 2 })
        ));
    }

    #[test]
    fn it_refuses_a_price_below_the_floor_the_collection_published() {
        let (s, mut e) = good();
        let mut b = bounds();
        b.min_mint_price_wei = PRICE + 1;
        e.bounds = Some(b);
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::PriceBelowFloor { .. })
        ));
    }

    #[test]
    fn it_refuses_a_stage_window_outside_the_published_one() {
        let (mut s, e) = good();
        s.data = Call {
            start: 1_000,
            ..Call::default()
        }
        .encode();
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::WindowOutsideBounds { .. })
        ));
    }

    #[test]
    fn it_refuses_a_fee_outside_the_published_range() {
        let (mut s, e) = good();
        s.data = Call {
            fee_bps: 5_000,
            ..Call::default()
        }
        .encode();
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::FeeBpsOutsideBounds { .. })
        ));
    }

    // Missing bounds is missing evidence, not permission: everything else in the
    // table still has to hold.
    #[test]
    fn it_still_checks_everything_else_when_no_bounds_were_readable() {
        let (s, mut e) = good();
        e.bounds = None;
        assert!(verify(&s, &e).is_ok());

        e.minter = "0x000000000000000000000000000000000000f00d"
            .parse()
            .unwrap();
        assert!(matches!(verify(&s, &e), Err(Rejection::Minter { .. })));
    }

    // A correct call we cannot afford is still refused, and the run ceiling is
    // the thing being protected rather than this one call.
    #[test]
    fn it_refuses_a_total_that_would_break_the_run_ceiling() {
        let (s, mut e) = good();
        e.spend_remaining_wei = 0;
        assert!(matches!(
            verify(&s, &e),
            Err(Rejection::OverSpendCeiling { .. })
        ));
    }

    #[test]
    fn a_free_stage_needs_no_headroom() {
        let (mut s, mut e) = good();
        s.data = Call {
            price: 0,
            ..Call::default()
        }
        .encode();
        s.value_wei = 0;
        e.unit_price_wei = 0;
        e.spend_remaining_wei = 0;
        assert!(verify(&s, &e).is_ok());
    }

    // Every rejection has to name the field and carry both values, because
    // "refused" on its own sends somebody to read our source at T-2 minutes.
    #[test]
    fn every_rejection_says_what_was_expected_and_what_arrived() {
        let (s, mut e) = good();
        e.minter = "0x000000000000000000000000000000000000f00d"
            .parse()
            .unwrap();
        let printed = verify(&s, &e).unwrap_err().to_string();
        assert!(printed.contains("f00d"), "the expected value is missing");
        assert!(printed.contains("00aa"), "the received value is missing");
    }
}
