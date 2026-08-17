// Wired up by `mint` in P5. Written now because the rules encoded here were
// learned from real measurement, and a port that rediscovers them later has
// lost the thing it was ported for.
#![allow(dead_code)]

use std::future::Future;
use std::time::{Duration, Instant};

/// What actually happened. Four states, and only one of them is a win.
///
/// The chain documents sequencer-level compliance screening, which forces a
/// fourth state beyond the usual three. It is the dangerous one because it looks
/// like success: an endpoint returns a hash and the transaction simply never
/// exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A receipt exists. `reverted` distinguishes landing and failing from never
    /// landing, and conflating those hides a bug.
    Included { reverted: bool },
    /// Every endpoint refused it, with their reason.
    Rejected { reason: String },
    /// Accepted, never seen by a read node, and the nonce never moved.
    Vanished { reason: String },
    /// Anything we cannot honestly call one of the other three.
    Dispatched { reason: String },
}

impl Outcome {
    #[must_use]
    pub const fn is_win(&self) -> bool {
        matches!(self, Self::Included { reverted: false })
    }

    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Included { .. } => "included",
            Self::Rejected { .. } => "rejected",
            Self::Vanished { .. } => "vanished",
            Self::Dispatched { .. } => "dispatched",
        }
    }
}

/// The reads classification needs. Separated behind a trait so the decision
/// logic can be tested against a slow or failing chain without one.
pub trait ChainProbe {
    /// `None` when there is no receipt yet. `Some(status)` when there is.
    fn receipt(&mut self) -> impl Future<Output = Result<Option<String>, ()>>;
    /// Whether any read node has heard of the transaction.
    fn seen(&mut self) -> impl Future<Output = Result<bool, ()>>;
    fn nonce(&mut self) -> impl Future<Output = Result<u64, ()>>;
    /// Push the same signed bytes through a different network path.
    fn resend(&mut self) -> impl Future<Output = Result<(), ()>>;
}

#[derive(Debug, Clone, Copy)]
pub struct ConfirmSettings {
    /// About five blocks at 101 ms. A transaction no read node has heard of
    /// after five blocks is already abnormal.
    pub unknown_after: Duration,
    pub give_up_after: Duration,
    pub poll_interval: Duration,
    /// How many times a read node must successfully answer "never heard of it"
    /// before we are willing to call a transaction vanished.
    ///
    /// Measured from us-east-2: the public RPC answers reads at 56 ms p50 but
    /// 468 ms p95, against a 500 ms window. Counting elapsed time would let a
    /// slow endpoint manufacture the most alarming verdict we have. Counting
    /// answers cannot.
    pub min_answers_for_vanished: u32,
}

impl Default for ConfirmSettings {
    fn default() -> Self {
        Self {
            unknown_after: Duration::from_millis(500),
            give_up_after: Duration::from_secs(3),
            poll_interval: Duration::from_millis(60),
            min_answers_for_vanished: 3,
        }
    }
}

/// Because `vanished` is the verdict a user reads as "nothing was minted", it is
/// the one that must never be reached on evidence we do not have. Everywhere
/// else this prefers to say it does not know.
pub async fn classify<P: ChainProbe>(
    probe: &mut P,
    nonce_before: u64,
    settings: ConfirmSettings,
) -> Outcome {
    let started = Instant::now();
    let mut resent = false;
    let mut ever_seen = false;
    // Successful reads that said the transaction does not exist. Only these
    // count as evidence.
    let mut denials: u32 = 0;
    let mut read_failures: u32 = 0;

    while started.elapsed() < settings.give_up_after {
        match probe.receipt().await {
            Ok(Some(status)) => {
                return Outcome::Included {
                    reverted: status == "0x0",
                };
            }
            Ok(None) => {}
            Err(()) => read_failures += 1,
        }

        match probe.seen().await {
            Ok(true) => ever_seen = true,
            Ok(false) => denials += 1,
            Err(()) => read_failures += 1,
        }

        if !ever_seen && !resent && started.elapsed() >= settings.unknown_after {
            resent = true;
            // A resend that fails must not decide the outcome either.
            let _ = probe.resend().await;
        }

        tokio::time::sleep(settings.poll_interval).await;
    }

    if ever_seen {
        return Outcome::Dispatched {
            reason: "known to the read nodes but not yet mined".to_owned(),
        };
    }

    // Not enough successful answers to conclude anything. Saying "vanished"
    // here would report a slow endpoint as a missing transaction, and a user
    // acting on that would believe a mint failed that may well have landed.
    if denials < settings.min_answers_for_vanished {
        return Outcome::Dispatched {
            reason: format!(
                "could not confirm: only {denials} read node answer{} and {read_failures} failed \
                 read{}. Check the transaction hash before assuming anything.",
                plural(denials),
                plural(read_failures),
            ),
        };
    }

    let Ok(nonce_now) = probe.nonce().await else {
        return Outcome::Dispatched {
            reason: format!(
                "{denials} read nodes never saw it, but the nonce could not be read, so \
                 whether anything was consumed is unknown."
            ),
        };
    };

    if nonce_now == nonce_before {
        Outcome::Vanished {
            reason: format!(
                "an endpoint accepted it, {denials} read node answers never saw it, and the \
                 nonce never advanced"
            ),
        }
    } else {
        Outcome::Dispatched {
            reason: "the nonce advanced but this hash was never seen, so something else landed"
                .to_owned(),
        }
    }
}

const fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain that behaves however a test needs it to.
    #[derive(Default)]
    struct Fake {
        receipt: Option<Result<Option<String>, ()>>,
        seen: Vec<Result<bool, ()>>,
        nonce: Option<Result<u64, ()>>,
        receipt_after: Option<(u32, String)>,
        calls: u32,
        resends: u32,
    }

    impl ChainProbe for Fake {
        async fn receipt(&mut self) -> Result<Option<String>, ()> {
            self.calls += 1;
            if let Some((at, status)) = &self.receipt_after {
                if self.calls >= *at {
                    return Ok(Some(status.clone()));
                }
                return Err(());
            }
            self.receipt.clone().unwrap_or(Ok(None))
        }
        async fn seen(&mut self) -> Result<bool, ()> {
            if self.seen.is_empty() {
                return Ok(false);
            }
            let i = (self.calls as usize - 1).min(self.seen.len() - 1);
            self.seen[i]
        }
        async fn nonce(&mut self) -> Result<u64, ()> {
            self.nonce.unwrap_or(Ok(7))
        }
        async fn resend(&mut self) -> Result<(), ()> {
            self.resends += 1;
            Ok(())
        }
    }

    fn fast() -> ConfirmSettings {
        ConfirmSettings {
            unknown_after: Duration::from_millis(20),
            give_up_after: Duration::from_millis(120),
            poll_interval: Duration::from_millis(10),
            min_answers_for_vanished: 3,
        }
    }

    #[tokio::test]
    async fn a_receipt_is_the_answer() {
        let mut chain = Fake {
            receipt: Some(Ok(Some("0x1".into()))),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        assert_eq!(out, Outcome::Included { reverted: false });
        assert!(out.is_win());
    }

    /// Landing and reverting is a different thing from never landing, and only
    /// one of them is a win.
    #[tokio::test]
    async fn a_revert_is_included_but_not_a_win() {
        let mut chain = Fake {
            receipt: Some(Ok(Some("0x0".into()))),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        assert_eq!(out, Outcome::Included { reverted: true });
        assert!(!out.is_win());
    }

    #[tokio::test]
    async fn read_nodes_that_answered_and_a_still_nonce_mean_vanished() {
        let mut chain = Fake {
            nonce: Some(Ok(7)),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        assert!(matches!(out, Outcome::Vanished { .. }));
        assert!(!out.is_win());
    }

    /// The one that matters. "vanished" is what a user reads as "nothing was
    /// minted", so it must never be reached because an endpoint was too slow.
    #[tokio::test]
    async fn never_vanished_when_the_reads_themselves_failed() {
        let mut chain = Fake {
            receipt: Some(Err(())),
            seen: vec![Err(())],
            nonce: Some(Ok(7)),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        assert!(matches!(out, Outcome::Dispatched { .. }), "got {out:?}");
        let Outcome::Dispatched { reason } = out else {
            unreachable!()
        };
        assert!(reason.contains("could not confirm"), "{reason}");
        assert!(reason.contains("failed read"), "{reason}");
    }

    /// Without the nonce we cannot tell whether anything was consumed, and
    /// saying vanished would assert exactly what we failed to measure.
    #[tokio::test]
    async fn never_vanished_when_the_nonce_could_not_be_read() {
        let mut chain = Fake {
            nonce: Some(Err(())),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        let Outcome::Dispatched { reason } = out else {
            panic!("expected dispatched")
        };
        assert!(reason.contains("nonce could not be read"), "{reason}");
    }

    #[tokio::test]
    async fn a_nonce_that_moved_means_something_else_landed() {
        let mut chain = Fake {
            nonce: Some(Ok(8)),
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        let Outcome::Dispatched { reason } = out else {
            panic!("expected dispatched")
        };
        assert!(reason.contains("something else landed"), "{reason}");
    }

    #[tokio::test]
    async fn a_transaction_the_nodes_can_see_is_not_vanished() {
        let mut chain = Fake {
            seen: vec![Ok(true)],
            ..Fake::default()
        };
        let out = classify(&mut chain, 7, fast()).await;
        let Outcome::Dispatched { reason } = out else {
            panic!("expected dispatched")
        };
        assert!(reason.contains("not yet mined"), "{reason}");
    }

    /// The second ingress is tried once, not on every poll.
    #[tokio::test]
    async fn resends_once_through_the_other_path() {
        let mut chain = Fake {
            nonce: Some(Ok(7)),
            ..Fake::default()
        };
        let _ = classify(&mut chain, 7, fast()).await;
        assert_eq!(chain.resends, 1);
    }

    /// A receipt arriving through the noise is still the answer.
    #[tokio::test]
    async fn a_receipt_wins_even_while_other_reads_are_failing() {
        let mut chain = Fake {
            receipt_after: Some((3, "0x1".into())),
            seen: vec![Err(())],
            ..Fake::default()
        };
        assert_eq!(
            classify(&mut chain, 7, fast()).await,
            Outcome::Included { reverted: false }
        );
    }
}
