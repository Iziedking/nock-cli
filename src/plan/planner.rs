use alloy_primitives::Address;

use super::spend::SpendCeiling;
use super::stage::Stage;
use crate::chain::opensea::verify::Rejection;

/// Who is in this stage, who is not, and why.
///
/// Pure. Everything it needs is passed in, so the rule that decides whether
/// somebody's money moves can be tested against situations that have not
/// happened rather than waited for.
///
/// THE PROMISE THIS KEEPS: every wallet in the set comes out the other side with
/// a status. Not most of them, not the ones that worked. A wallet that quietly
/// disappears between the set file and the report is the failure the Path B
/// nonce bug taught us to refuse, because the person holding it believes they
/// are in a drop they are not in.
#[derive(Debug, Clone)]
pub enum PlanStatus {
    /// Verified, affordable, and ready to sign.
    Ready { quantity: u64, cost_wei: u128 },
    /// The stage answered that this wallet is not on the list.
    NotEligible,
    /// The calldata did not survive verification. Carries the reason so the
    /// report can name the field and both values.
    Refused(Rejection),
    /// The wallet cannot cover price plus gas.
    Underfunded { needed_wei: u128, held_wei: u128 },
    /// Everything was fine and the run had no headroom left. Named separately
    /// from underfunded because the wallet did nothing wrong and the number that
    /// stopped it was one the user chose.
    DroppedForSpend {
        needed_wei: u128,
        remaining_wei: u128,
    },
}

impl PlanStatus {
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// One word for the report's status column.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Ready { .. } => "ready",
            Self::NotEligible => "not eligible",
            Self::Refused(_) => "refused",
            Self::Underfunded { .. } => "underfunded",
            Self::DroppedForSpend { .. } => "dropped for spend",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WalletPlan {
    /// Its place in the wallet set file, which is its place in the batch.
    pub index: usize,
    pub address: Address,
    pub status: PlanStatus,
}

#[derive(Debug, Clone)]
pub struct StagePlan {
    pub stage: Stage,
    pub wallets: Vec<WalletPlan>,
}

impl StagePlan {
    pub fn ready(&self) -> impl Iterator<Item = &WalletPlan> {
        self.wallets.iter().filter(|w| w.status.is_ready())
    }

    pub fn total_cost_wei(&self) -> u128 {
        self.wallets
            .iter()
            .map(|w| match w.status {
                PlanStatus::Ready { cost_wei, .. } => cost_wei,
                _ => 0,
            })
            .sum()
    }
}

/// One wallet's situation, gathered by the caller.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub index: usize,
    pub address: Address,
    /// What the stage said about this wallet. `false` covers `OpenSea`'s `null`,
    /// which is what an unlisted wallet gets.
    pub eligible: bool,
    /// What this wallet may mint here, already clamped by the caller against the
    /// stage cap and whatever the user asked for.
    pub quantity: u64,
    /// Verification's verdict on this wallet's calldata, if it got that far.
    pub refusal: Option<Rejection>,
    pub balance_wei: u128,
    /// The worst case gas for one send, so the funding check covers both halves.
    pub gas_ceiling_wei: u128,
}

/// Decides each wallet's fate, in set order, against a ceiling it draws down.
///
/// Order is the user's decision. When the money runs out it runs out at the
/// bottom of their file, not at whichever wallet happened to be cheapest or
/// quickest, so who loses their place was settled when they wrote it down.
pub fn build_plan(stage: Stage, candidates: &[Candidate], ceiling: &mut SpendCeiling) -> StagePlan {
    let mut wallets = Vec::with_capacity(candidates.len());

    for candidate in candidates {
        let status = plan_one(stage, candidate, ceiling);
        wallets.push(WalletPlan {
            index: candidate.index,
            address: candidate.address,
            status,
        });
    }

    StagePlan { stage, wallets }
}

fn plan_one(stage: Stage, candidate: &Candidate, ceiling: &mut SpendCeiling) -> PlanStatus {
    // Refusal first. A wallet whose calldata failed verification is out
    // regardless of what it could afford, and saying "underfunded" about a
    // wallet we would never have signed for would be a lie about the reason.
    if let Some(refusal) = candidate.refusal.clone() {
        return PlanStatus::Refused(refusal);
    }
    if !candidate.eligible {
        return PlanStatus::NotEligible;
    }
    if candidate.quantity == 0 {
        return PlanStatus::NotEligible;
    }

    let cost_wei = stage
        .price_wei
        .saturating_mul(u128::from(candidate.quantity));
    let needed_wei = cost_wei.saturating_add(candidate.gas_ceiling_wei);

    if candidate.balance_wei < needed_wei {
        return PlanStatus::Underfunded {
            needed_wei,
            held_wei: candidate.balance_wei,
        };
    }

    // The ceiling is drawn down only by wallets that are otherwise ready, so an
    // ineligible or underfunded wallet never consumes headroom a later one could
    // have used.
    if ceiling.commit(cost_wei).is_err() {
        return PlanStatus::DroppedForSpend {
            needed_wei: cost_wei,
            remaining_wei: ceiling.remaining(),
        };
    }

    PlanStatus::Ready {
        quantity: candidate.quantity,
        cost_wei,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::opensea::gql::StageType;

    const ETH: u128 = 1_000_000_000_000_000_000;
    const GAS: u128 = ETH / 1000;

    fn stage(price_wei: u128) -> Stage {
        Stage {
            index: 1,
            kind: StageType::SignedPresale,
            start_time: 1_000,
            end_time: 5_000,
            price_wei,
            max_per_wallet: 5,
        }
    }

    fn candidate(index: usize) -> Candidate {
        Candidate {
            index,
            address: Address::repeat_byte(u8::try_from(index + 1).unwrap()),
            eligible: true,
            quantity: 1,
            refusal: None,
            balance_wei: ETH,
            gas_ceiling_wei: GAS,
        }
    }

    // The promise. Four in, four out, whatever happened to any of them.
    #[test]
    fn every_wallet_in_the_set_appears_exactly_once() {
        let candidates: Vec<_> = (0..4).map(candidate).collect();
        let plan = build_plan(stage(ETH / 100), &candidates, &mut SpendCeiling::new(ETH));
        assert_eq!(plan.wallets.len(), 4);
        let mut seen: Vec<usize> = plan.wallets.iter().map(|w| w.index).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }

    #[test]
    fn it_marks_everyone_ready_when_the_run_can_afford_them() {
        let candidates: Vec<_> = (0..3).map(candidate).collect();
        let plan = build_plan(stage(ETH / 100), &candidates, &mut SpendCeiling::new(ETH));
        assert_eq!(plan.ready().count(), 3);
        assert_eq!(plan.total_cost_wei(), 3 * (ETH / 100));
    }

    // Set order decides who loses their place, because that order is something
    // the user wrote down in advance.
    #[test]
    fn the_money_runs_out_at_the_bottom_of_the_file() {
        let candidates: Vec<_> = (0..4).map(candidate).collect();
        let plan = build_plan(
            stage(ETH / 10),
            &candidates,
            &mut SpendCeiling::new(ETH / 4),
        );
        assert!(plan.wallets[0].status.is_ready());
        assert!(plan.wallets[1].status.is_ready());
        assert!(matches!(
            plan.wallets[2].status,
            PlanStatus::DroppedForSpend { .. }
        ));
        assert!(matches!(
            plan.wallets[3].status,
            PlanStatus::DroppedForSpend { .. }
        ));
    }

    #[test]
    fn an_ineligible_wallet_is_named_rather_than_dropped() {
        let mut candidates: Vec<_> = (0..3).map(candidate).collect();
        candidates[1].eligible = false;
        let plan = build_plan(stage(0), &candidates, &mut SpendCeiling::new(0));
        assert!(matches!(plan.wallets[1].status, PlanStatus::NotEligible));
        assert_eq!(plan.ready().count(), 2);
    }

    // A refusal is about trust, not money, so it is reported as a refusal even
    // when the wallet could not have afforded it either.
    #[test]
    fn a_refused_wallet_reports_the_refusal_and_not_a_money_problem() {
        let mut candidates: Vec<_> = (0..2).map(candidate).collect();
        candidates[0].balance_wei = 0;
        candidates[0].refusal = Some(Rejection::Minter {
            expected: Address::ZERO,
            got: Address::repeat_byte(9),
        });
        let plan = build_plan(stage(ETH / 100), &candidates, &mut SpendCeiling::new(ETH));
        assert!(matches!(plan.wallets[0].status, PlanStatus::Refused(_)));
        assert!(plan.wallets[1].status.is_ready());
    }

    // Gas and price together, because a wallet that can pay the price and not
    // the gas mints nothing.
    #[test]
    fn a_wallet_is_underfunded_when_it_cannot_cover_price_and_gas_together() {
        let mut candidates: Vec<_> = (0..2).map(candidate).collect();
        candidates[0].balance_wei = ETH / 100;
        let plan = build_plan(stage(ETH / 100), &candidates, &mut SpendCeiling::new(ETH));
        match plan.wallets[0].status {
            PlanStatus::Underfunded {
                needed_wei,
                held_wei,
            } => {
                assert_eq!(needed_wei, ETH / 100 + GAS);
                assert_eq!(held_wei, ETH / 100);
            }
            ref other => panic!("expected underfunded, got {other:?}"),
        }
    }

    // An ineligible or broke wallet must not eat headroom a later wallet could
    // have used.
    #[test]
    fn only_ready_wallets_draw_down_the_ceiling() {
        let mut candidates: Vec<_> = (0..3).map(candidate).collect();
        candidates[0].eligible = false;
        candidates[1].balance_wei = 0;
        let mut ceiling = SpendCeiling::new(ETH / 10);
        let plan = build_plan(stage(ETH / 10), &candidates, &mut ceiling);
        assert!(plan.wallets[2].status.is_ready());
        assert_eq!(ceiling.remaining(), 0);
    }

    // A free stage needs no headroom at all, so a run with no --max-spend still
    // works for the half of the market that costs nothing.
    #[test]
    fn a_free_stage_needs_no_ceiling() {
        let candidates: Vec<_> = (0..5).map(candidate).collect();
        let plan = build_plan(stage(0), &candidates, &mut SpendCeiling::new(0));
        assert_eq!(plan.ready().count(), 5);
        assert_eq!(plan.total_cost_wei(), 0);
    }

    #[test]
    fn a_quantity_of_zero_is_not_a_place_in_the_batch() {
        let mut candidates: Vec<_> = (0..2).map(candidate).collect();
        candidates[0].quantity = 0;
        let plan = build_plan(stage(0), &candidates, &mut SpendCeiling::new(0));
        assert!(matches!(plan.wallets[0].status, PlanStatus::NotEligible));
    }

    // The report prints these, so they have to read as English rather than as
    // enum variants.
    #[test]
    fn every_status_has_a_label_for_the_report() {
        let mut candidates: Vec<_> = (0..3).map(candidate).collect();
        candidates[0].eligible = false;
        candidates[1].balance_wei = 0;
        let plan = build_plan(stage(ETH / 100), &candidates, &mut SpendCeiling::new(ETH));
        let labels: Vec<_> = plan.wallets.iter().map(|w| w.status.label()).collect();
        assert_eq!(labels, vec!["not eligible", "underfunded", "ready"]);
    }
}
