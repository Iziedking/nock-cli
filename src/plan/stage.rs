use crate::chain::opensea::gql::{StageMeta, StageType};

/// One mint window, and what it means when it moves.
///
/// A drop is several stages: a gated one, then an allowlist, then public. A run
/// walks them, and between planning a stage and firing it anything can change.
/// This is the part that notices.
///
/// THE DISTINCTION THE WHOLE LOOP RESTS ON. An EIP-1559 transaction carries no
/// timestamp, so a public stage sliding from 14:00 to 16:00 leaves the signed
/// bytes perfectly valid and only changes when we broadcast them. A signed stage
/// sliding does not: `MintParams` carries `startTime` and `endTime`, the signed
/// digest binds both, and the old signature is dead the moment either moves.
///
/// Getting that backwards costs either a pointless re-fetch on every public
/// reschedule, or a batch of signatures the contract will refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stage {
    pub index: u64,
    pub kind: StageType,
    pub start_time: u64,
    pub end_time: u64,
    pub price_wei: u128,
    pub max_per_wallet: u64,
}

impl Stage {
    pub const fn is_signed(&self) -> bool {
        self.kind.is_signed()
    }

    /// Whether this tool can mint this stage at all.
    ///
    /// Merkle allowlists cannot. `mintAllowList` takes a proof this tool does
    /// not build, and `is_signed` is false for them, so without this check a
    /// merkle stage falls through to the public path and `mintPublic` calldata
    /// gets built for an allowlist stage. That either reverts, which wastes the
    /// slot, or mints the public phase by accident at the public price, which is
    /// worse because it succeeds.
    ///
    /// Measured on chain 4663: 50 of 52 collections gate with a signer, so this
    /// refuses a rounding error rather than a market.
    pub const fn is_mintable(&self) -> bool {
        !matches!(self.kind, StageType::MerklePresale)
    }

    pub const fn is_open_at(&self, unix_seconds: u64) -> bool {
        unix_seconds >= self.start_time && unix_seconds < self.end_time
    }

    /// Built from what `OpenSea` published, with the price supplied separately.
    ///
    /// The price does not come from the metadata query: it is either the public
    /// stage price read from chain, or the price inside verified calldata. Both
    /// are sources we can check, and the metadata is not.
    pub const fn from_meta(meta: &StageMeta, price_wei: u128) -> Self {
        Self {
            index: meta.stage_index,
            kind: meta.stage_type,
            start_time: meta.start_time,
            end_time: meta.end_time,
            price_wei,
            max_per_wallet: meta.max_total_mintable_by_wallet,
        }
    }
}

/// Something that moved between planning and firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// One wallet sent a transaction from somewhere else, so its nonce is stale
    /// and the signature bound to it is dead. Cheap to fix here because we hold
    /// the key; this is the case product B cannot fix by itself.
    NonceMoved {
        index: usize,
    },
    /// A public stage was rescheduled. Re-time the broadcast, keep the bytes.
    PublicStageMoved {
        start_time: u64,
    },
    /// A signed stage was rescheduled. The signature is dead: re-fetch, re-verify
    /// and re-sign.
    SignedStageMoved,
    PriceUp {
        price_wei: u128,
    },
    CapDown {
        max_per_wallet: u64,
    },
}

/// Whether this drift means a wallet's transaction has to be signed again.
///
/// A public reschedule does not, which is the point of tracking the two kinds
/// separately. Nothing here re-signs on a hunch.
pub const fn requires_resign(drift: &Drift) -> bool {
    // Written as one arm because clippy is right that the bodies are the same,
    // but the interesting half is the exception: a public reschedule is the only
    // drift that leaves already-signed bytes valid.
    !matches!(drift, Drift::PublicStageMoved { .. })
}

/// Everything that moved, in one pass.
///
/// Returns a list rather than the first thing found, because a stage can be
/// rescheduled and repriced at once and a caller that only heard about one
/// would re-plan for it and fire into the other.
pub fn detect_drift(
    before: &Stage,
    after: &Stage,
    nonces_before: &[u64],
    nonces_after: &[u64],
) -> Vec<Drift> {
    let mut out = Vec::new();

    if before.start_time != after.start_time || before.end_time != after.end_time {
        if after.is_signed() {
            out.push(Drift::SignedStageMoved);
        } else {
            out.push(Drift::PublicStageMoved {
                start_time: after.start_time,
            });
        }
    }

    // Only upwards. A price that fell costs nobody anything and re-planning for
    // it would spend the freeze window on good news.
    if after.price_wei > before.price_wei {
        out.push(Drift::PriceUp {
            price_wei: after.price_wei,
        });
    }

    // Only downwards, for the same reason in reverse: a raised cap does not
    // invalidate a quantity anybody already asked for.
    if after.max_per_wallet < before.max_per_wallet {
        out.push(Drift::CapDown {
            max_per_wallet: after.max_per_wallet,
        });
    }

    for (index, (was, now)) in nonces_before.iter().zip(nonces_after.iter()).enumerate() {
        if was != now {
            out.push(Drift::NonceMoved { index });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(start: u64) -> Stage {
        Stage {
            index: 0,
            kind: StageType::PublicSale,
            start_time: start,
            end_time: start + 3600,
            price_wei: 0,
            max_per_wallet: 3,
        }
    }

    fn signed(start: u64) -> Stage {
        Stage {
            kind: StageType::SignedPresale,
            ..public(start)
        }
    }

    // An EIP-1559 transaction carries no timestamp, so the signed bytes survive
    // a public reschedule untouched. Only the broadcast moves.
    #[test]
    fn a_public_stage_moving_does_not_need_a_new_signature() {
        let drift = detect_drift(&public(100), &public(200), &[1], &[1]);
        assert_eq!(drift, vec![Drift::PublicStageMoved { start_time: 200 }]);
        assert!(!requires_resign(&drift[0]));
    }

    // MintParams carries startTime and endTime and the signed digest binds both,
    // so the old signature is dead the moment either moves.
    #[test]
    fn a_signed_stage_moving_kills_the_signature() {
        let drift = detect_drift(&signed(100), &signed(200), &[1], &[1]);
        assert_eq!(drift, vec![Drift::SignedStageMoved]);
        assert!(requires_resign(&drift[0]));
    }

    #[test]
    fn an_end_time_moving_counts_as_the_stage_moving() {
        let mut after = signed(100);
        after.end_time += 60;
        assert_eq!(
            detect_drift(&signed(100), &after, &[1], &[1]),
            vec![Drift::SignedStageMoved]
        );
    }

    // The case product B cannot fix by itself. Here we hold the key, so it costs
    // one re-sign for one wallet rather than the whole batch.
    #[test]
    fn a_moved_nonce_names_only_the_wallet_it_moved_for() {
        let drift = detect_drift(&public(100), &public(100), &[7, 7, 7], &[7, 8, 7]);
        assert_eq!(drift, vec![Drift::NonceMoved { index: 1 }]);
        assert!(requires_resign(&drift[0]));
    }

    #[test]
    fn several_moved_nonces_are_all_reported() {
        let drift = detect_drift(&public(100), &public(100), &[1, 1, 1], &[2, 1, 9]);
        assert_eq!(
            drift,
            vec![
                Drift::NonceMoved { index: 0 },
                Drift::NonceMoved { index: 2 }
            ]
        );
    }

    #[test]
    fn a_price_rise_has_to_be_re_costed() {
        let mut after = public(100);
        after.price_wei = 5;
        assert_eq!(
            detect_drift(&public(100), &after, &[1], &[1]),
            vec![Drift::PriceUp { price_wei: 5 }]
        );
    }

    // Good news is not drift. Re-planning for a price that fell would spend the
    // freeze window on something nobody needed.
    #[test]
    fn a_price_that_fell_is_not_drift() {
        let mut before = public(100);
        before.price_wei = 10;
        assert!(detect_drift(&before, &public(100), &[1], &[1]).is_empty());
    }

    #[test]
    fn a_lower_cap_is_drift_and_a_higher_one_is_not() {
        let mut lower = public(100);
        lower.max_per_wallet = 1;
        assert_eq!(
            detect_drift(&public(100), &lower, &[1], &[1]),
            vec![Drift::CapDown { max_per_wallet: 1 }]
        );

        let mut higher = public(100);
        higher.max_per_wallet = 9;
        assert!(detect_drift(&public(100), &higher, &[1], &[1]).is_empty());
    }

    // A stage can be rescheduled and repriced at once. A caller told about only
    // one of those would re-plan for it and fire into the other.
    #[test]
    fn it_reports_everything_that_moved_rather_than_the_first_thing() {
        let mut after = signed(200);
        after.price_wei = 5;
        after.max_per_wallet = 1;
        let drift = detect_drift(&signed(100), &after, &[1], &[2]);
        assert_eq!(drift.len(), 4);
        assert!(drift.contains(&Drift::SignedStageMoved));
        assert!(drift.contains(&Drift::PriceUp { price_wei: 5 }));
        assert!(drift.contains(&Drift::CapDown { max_per_wallet: 1 }));
        assert!(drift.contains(&Drift::NonceMoved { index: 0 }));
    }

    #[test]
    fn nothing_moving_is_no_drift() {
        assert!(detect_drift(&public(100), &public(100), &[1, 2], &[1, 2]).is_empty());
    }

    // Without this a merkle stage reads as "not signed", falls through to the
    // public path, and mintPublic calldata is built for an allowlist stage.
    #[test]
    fn a_merkle_stage_is_refused_rather_than_treated_as_public() {
        let merkle = Stage {
            kind: StageType::MerklePresale,
            ..public(100)
        };
        assert!(!merkle.is_signed(), "it is genuinely not a signed stage");
        assert!(
            !merkle.is_mintable(),
            "and it must not be minted as a public one"
        );
    }

    #[test]
    fn the_two_stages_this_tool_serves_are_mintable() {
        assert!(public(100).is_mintable());
        assert!(signed(100).is_mintable());
    }

    #[test]
    fn a_stage_knows_when_it_is_open() {
        let s = public(100);
        assert!(!s.is_open_at(99));
        assert!(s.is_open_at(100));
        assert!(s.is_open_at(3_699));
        assert!(!s.is_open_at(3_700), "the end is exclusive");
    }
}
