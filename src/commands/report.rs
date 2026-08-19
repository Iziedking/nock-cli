#![allow(dead_code)]

use std::fmt::Write as _;
use std::process::ExitCode;

use alloy_primitives::Address;

use crate::commands::doctor::format_eth;
use crate::engine::confirm::Outcome;
use crate::plan::planner::{PlanStatus, StagePlan};
use crate::plan::spend::SpendCeiling;

/// What the run tells you, before and after it spends anything.
///
/// The table is the real output. The exit code exists so a script can ask one
/// question, and answers only that question: did anything mint.
///
/// EVERY WALLET APPEARS EXACTLY ONCE, including the ones dropped during
/// planning, with the reason. A wallet that quietly vanishes between the set
/// file and the report is the failure the Path B nonce bug taught us to refuse:
/// its owner believes they are in a drop they are not in.
///
/// Consumed by the mint command, which is the next thing to land.
const SHORT: usize = 10;

fn short(address: Address) -> String {
    let full = format!("{address:?}");
    format!("{}…{}", &full[..SHORT], &full[full.len() - 4..])
}

/// The status column, with the arithmetic when there is any.
fn detail(status: &PlanStatus) -> String {
    match status {
        PlanStatus::Ready { .. } => "ready".to_owned(),
        PlanStatus::NotEligible => "not eligible for this stage".to_owned(),
        PlanStatus::SoldOut { left, wanted } => {
            format!("sold out: {left} left and {wanted} wanted")
        }
        // Named field, both values. "Refused" alone sends somebody to read our
        // source at T-2 minutes.
        PlanStatus::Refused(why) => format!("refused: {why}"),
        PlanStatus::Underfunded {
            needed_wei,
            held_wei,
        } => format!(
            "underfunded: needs {} ETH, holds {} ETH",
            format_eth(*needed_wei),
            format_eth(*held_wei)
        ),
        PlanStatus::DroppedForSpend {
            needed_wei,
            remaining_wei,
        } => format!(
            "dropped: needs {} ETH, {} ETH of --max-spend left",
            format_eth(*needed_wei),
            format_eth(*remaining_wei)
        ),
    }
}

/// What the run intends, printed before a key signs anything.
pub fn render_plan_table(plan: &StagePlan, ceiling: &SpendCeiling) -> String {
    let mut out = String::new();
    let kind = if plan.stage.is_signed() {
        "signed"
    } else {
        "public"
    };
    let _ = writeln!(
        out,
        "\n  stage {} ({kind}), opens at unix {}, {} per wallet\n",
        plan.stage.index, plan.stage.start_time, plan.stage.max_per_wallet
    );
    out.push_str("  #   wallet              qty   cost         status\n");

    for wallet in &plan.wallets {
        let (qty, cost) = match &wallet.status {
            PlanStatus::Ready { quantity, cost_wei } => {
                (quantity.to_string(), format_eth(*cost_wei))
            }
            _ => ("-".to_owned(), "-".to_owned()),
        };
        let _ = writeln!(
            out,
            "  {:<3} {:<19} {qty:<5} {cost:<12} {}",
            wallet.index,
            short(wallet.address),
            detail(&wallet.status)
        );
    }

    let _ = writeln!(
        out,
        "\n  {} of {} ready, {} ETH committed, {} ETH of --max-spend left",
        plan.ready().count(),
        plan.wallets.len(),
        format_eth(plan.total_cost_wei()),
        format_eth(ceiling.remaining())
    );
    out
}

pub struct WalletOutcome {
    pub index: usize,
    pub address: Address,
    pub outcome: Outcome,
    pub tx_hash: Option<String>,
}

/// What actually happened, in the four states and no fifth.
pub fn render_outcome_table(results: &[WalletOutcome]) -> String {
    let mut out = String::from("\n  #   wallet              result\n");
    for r in results {
        let note = match &r.outcome {
            // A dispatch is never dressed up as a win. It is a transaction an
            // endpoint accepted and nothing has confirmed.
            Outcome::Included { reverted: true } => {
                "included but reverted, nothing was minted".to_owned()
            }
            Outcome::Included { reverted: false } => "minted".to_owned(),
            Outcome::Rejected { reason } => format!("rejected: {reason}"),
            Outcome::Vanished { reason } => format!("vanished: {reason}"),
            Outcome::Dispatched { reason } => format!("dispatched, no receipt yet: {reason}"),
        };
        let hash = r
            .tx_hash
            .as_ref()
            .map(|h| format!("  {}", &h[..h.len().min(12)]))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {:<3} {:<19} {note}{hash}",
            r.index,
            short(r.address)
        );
    }
    out
}

/// Zero when at least one wallet minted, one when none did.
///
/// Partial success is the ordinary case for a wallet set and must not read as
/// failure: three of eight minting is three more than not running. A dispatch
/// with no receipt is not success, and neither is an inclusion that reverted.
pub fn exit_code(results: &[WalletOutcome]) -> ExitCode {
    if results.iter().any(|r| r.outcome.is_win()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::opensea::gql::StageType;
    use crate::chain::opensea::verify::Rejection;
    use crate::plan::planner::WalletPlan;
    use crate::plan::stage::Stage;

    const ETH: u128 = 1_000_000_000_000_000_000;

    fn outcome(index: usize, outcome: Outcome) -> WalletOutcome {
        WalletOutcome {
            index,
            address: Address::repeat_byte(u8::try_from(index + 1).unwrap()),
            outcome,
            tx_hash: None,
        }
    }

    fn plan_with_every_status() -> StagePlan {
        let statuses = vec![
            PlanStatus::Ready {
                quantity: 2,
                cost_wei: ETH / 100,
            },
            PlanStatus::NotEligible,
            PlanStatus::Underfunded {
                needed_wei: ETH,
                held_wei: 0,
            },
            PlanStatus::DroppedForSpend {
                needed_wei: ETH,
                remaining_wei: 1,
            },
            PlanStatus::Refused(Rejection::Minter {
                expected: Address::repeat_byte(0xaa),
                got: Address::repeat_byte(0xbb),
            }),
        ];
        StagePlan {
            stage: Stage {
                index: 1,
                kind: StageType::SignedPresale,
                start_time: 1_786_966_789,
                end_time: 1_787_226_889,
                price_wei: ETH / 200,
                max_per_wallet: 5,
            },
            wallets: statuses
                .into_iter()
                .enumerate()
                .map(|(index, status)| WalletPlan {
                    index,
                    address: Address::repeat_byte(u8::try_from(index + 1).unwrap()),
                    status,
                })
                .collect(),
        }
    }

    // Partial success is the ordinary case for a wallet set. Three of eight
    // minting is three more than not running, and must not read as failure.
    #[test]
    fn one_wallet_minting_is_success_even_when_others_did_not() {
        let results = vec![
            outcome(0, Outcome::Included { reverted: false }),
            outcome(
                1,
                Outcome::Rejected {
                    reason: "nope".to_owned(),
                },
            ),
        ];
        assert_eq!(
            format!("{:?}", exit_code(&results)),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    #[test]
    fn nothing_minting_is_failure() {
        let results = vec![outcome(
            0,
            Outcome::Rejected {
                reason: "nope".to_owned(),
            },
        )];
        assert_eq!(
            format!("{:?}", exit_code(&results)),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    // A transaction an endpoint accepted is not a mint. This is the state the
    // whole confirmation layer exists to keep honest.
    #[test]
    fn a_dispatch_without_a_receipt_is_not_success() {
        let results = vec![outcome(
            0,
            Outcome::Dispatched {
                reason: "no receipt yet".to_owned(),
            },
        )];
        assert_eq!(
            format!("{:?}", exit_code(&results)),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    #[test]
    fn an_inclusion_that_reverted_is_not_success() {
        let results = vec![outcome(0, Outcome::Included { reverted: true })];
        assert_eq!(
            format!("{:?}", exit_code(&results)),
            format!("{:?}", ExitCode::FAILURE)
        );
        assert!(render_outcome_table(&results).contains("reverted"));
    }

    #[test]
    fn an_empty_run_is_failure_because_nothing_minted() {
        assert_eq!(
            format!("{:?}", exit_code(&[])),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    // The promise, in the output rather than only in the types.
    #[test]
    fn the_plan_table_names_every_wallet_including_the_dropped_ones() {
        let plan = plan_with_every_status();
        let table = render_plan_table(&plan, &SpendCeiling::new(ETH));
        for wallet in &plan.wallets {
            let tag = short(wallet.address);
            assert!(
                table.contains(&tag),
                "wallet {} is missing from the table",
                wallet.index
            );
        }
        assert_eq!(table.lines().filter(|l| l.contains('…')).count(), 5);
    }

    // Every reason a wallet is out has to say why in words, with the numbers.
    #[test]
    fn the_plan_table_gives_a_reason_for_every_wallet_that_is_out() {
        let table = render_plan_table(&plan_with_every_status(), &SpendCeiling::new(ETH));
        assert!(table.contains("not eligible"));
        assert!(table.contains("underfunded: needs"));
        assert!(table.contains("dropped: needs"));
        assert!(table.contains("refused:"));
        // The refusal carries both values, not just the word.
        assert!(table.contains("aaaa"));
        assert!(table.contains("bbbb"));
    }

    #[test]
    fn the_plan_table_shows_what_the_run_committed_and_what_is_left() {
        let mut ceiling = SpendCeiling::new(ETH);
        ceiling.commit(ETH / 100).unwrap();
        let table = render_plan_table(&plan_with_every_status(), &ceiling);
        assert!(table.contains("1 of 5 ready"));
        assert!(table.contains("of --max-spend left"));
    }

    #[test]
    fn the_plan_table_says_whether_the_stage_is_signed() {
        let table = render_plan_table(&plan_with_every_status(), &SpendCeiling::new(ETH));
        assert!(table.contains("signed"));
    }

    // Four states, no fifth, and nothing folded into "pending".
    #[test]
    fn the_outcome_table_uses_the_four_states_and_no_others() {
        let results = vec![
            outcome(0, Outcome::Included { reverted: false }),
            outcome(
                1,
                Outcome::Rejected {
                    reason: "refused".to_owned(),
                },
            ),
            outcome(
                2,
                Outcome::Vanished {
                    reason: "never landed".to_owned(),
                },
            ),
            outcome(
                3,
                Outcome::Dispatched {
                    reason: "no receipt".to_owned(),
                },
            ),
        ];
        let table = render_outcome_table(&results);
        assert!(table.contains("minted"));
        assert!(table.contains("rejected"));
        assert!(table.contains("vanished"));
        assert!(table.contains("dispatched, no receipt yet"));
        assert!(!table.contains("pending"));
    }
}
