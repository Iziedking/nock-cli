//! Built ahead of its caller, like the wallet set beside it. The mint command
//! is what dispatches a batch; this is only the part that makes the dispatch
//! concurrent. One allow, deleted in one edit when that lands.
#![allow(dead_code)]
use std::future::Future;

use alloy_primitives::Address;

use crate::chain::tx::Signed;

/// Sending a whole wallet set at once.
///
/// The only edge this tool has is being early, and it is measured in
/// milliseconds. Sending eight wallets one after another spends a round trip on
/// each before the next one starts, so the last wallet in the set arrives after
/// seven round trips it had no part in. On a chain that sequences first come,
/// first served, that is the difference between a place and no place.
///
/// So every send is dispatched before any reply is awaited. The test that
/// matters here is not that the results come back, it is that nothing waits its
/// turn: a barrier that only releases once all of them have started will hang
/// forever against an implementation that serializes.
///
/// This module sends. It does not decide what happened, because a transaction
/// the endpoint accepted has not necessarily landed, and that judgement belongs
/// with the confirmation code that watches for a receipt.
#[derive(Debug, Clone)]
pub struct Shot {
    /// Position in the wallet set, carried through so the report can put every
    /// wallet back in the order the user wrote them.
    pub index: usize,
    pub address: Address,
    pub nonce: u64,
    pub signed: Signed,
}

#[derive(Debug, Clone)]
pub struct ShotResult {
    pub index: usize,
    pub address: Address,
    pub nonce: u64,
    /// The transaction hash an endpoint accepted, or why none would take it.
    /// Deliberately not an outcome: acceptance is not inclusion.
    pub dispatch: Result<String, String>,
}

/// Dispatches every shot at once and returns one result per shot, in set order.
///
/// `send` is taken by value per shot so each call owns everything it needs and
/// can run on its own task. Results are sorted before returning, so the order of
/// the report never depends on which endpoint answered first.
pub async fn fire_all_with<S, F>(shots: Vec<Shot>, send: S) -> Vec<ShotResult>
where
    S: Fn(Shot) -> F + Clone + Send + 'static,
    F: Future<Output = Result<String, String>> + Send + 'static,
{
    // Every task is spawned first, in one pass, and only then are any of them
    // awaited. Collecting the handles is what makes that true: awaiting inside
    // this loop would be the serial version wearing a concurrent shape.
    let mut handles = Vec::with_capacity(shots.len());
    for shot in shots {
        let send = send.clone();
        let index = shot.index;
        let address = shot.address;
        let nonce = shot.nonce;
        handles.push((
            index,
            address,
            nonce,
            tokio::spawn(async move { send(shot).await }),
        ));
    }

    let mut out = Vec::with_capacity(handles.len());
    for (index, address, nonce, handle) in handles {
        let dispatch = match handle.await {
            Ok(result) => result,
            // A panicked task is one wallet's problem. Reporting it as a failed
            // dispatch keeps the promise that every wallet appears exactly once.
            Err(err) => Err(format!("the send task did not finish: {err}")),
        };
        out.push(ShotResult {
            index,
            address,
            nonce,
            dispatch,
        });
    }
    out.sort_by_key(|r| r.index);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn shot(index: usize) -> Shot {
        Shot {
            index,
            address: Address::ZERO,
            nonce: index as u64,
            signed: Signed {
                hash: Default::default(),
                raw: vec![1, 2, 3],
            },
        }
    }

    // The property this module exists for. Each stub send parks on a barrier
    // that only releases once all eight have arrived, so an implementation that
    // waits for one reply before starting the next never reaches the eighth and
    // the test hangs rather than passing slowly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn it_dispatches_every_wallet_before_awaiting_any_reply() {
        let n = 8;
        let barrier = Arc::new(Barrier::new(n));
        let started = Arc::new(AtomicUsize::new(0));

        let results = {
            let barrier = barrier.clone();
            let started = started.clone();
            fire_all_with((0..n).map(shot).collect(), move |s: Shot| {
                let barrier = barrier.clone();
                let started = started.clone();
                async move {
                    started.fetch_add(1, Ordering::SeqCst);
                    barrier.wait().await;
                    Ok(format!("0x{:064x}", s.index))
                }
            })
            .await
        };

        assert_eq!(results.len(), n);
        assert_eq!(started.load(Ordering::SeqCst), n);
        assert!(results.iter().all(|r| r.dispatch.is_ok()));
    }

    // One endpoint refusing one wallet is one wallet's problem. The Path B nonce
    // bug is why a wallet may never quietly vanish from the results.
    #[tokio::test]
    async fn it_reports_a_failed_send_without_losing_the_others() {
        let results = fire_all_with((0..3).map(shot).collect(), |s: Shot| async move {
            if s.index == 1 {
                Err("every endpoint refused it".to_owned())
            } else {
                Ok("0xabc".to_owned())
            }
        })
        .await;

        assert_eq!(results.len(), 3);
        assert!(results[0].dispatch.is_ok());
        assert_eq!(
            results[1].dispatch.as_ref().unwrap_err(),
            "every endpoint refused it"
        );
        assert!(results[2].dispatch.is_ok());
    }

    // Set order is the order the report reads in, whatever order the network
    // answered in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn it_returns_results_in_set_order_not_completion_order() {
        let results = fire_all_with((0..6).map(shot).collect(), |s: Shot| async move {
            // The later a wallet sits in the set, the faster it answers, so
            // completion order is the reverse of set order.
            tokio::time::sleep(std::time::Duration::from_millis((10 - s.index as u64) * 5)).await;
            Ok(format!("0x{:x}", s.index))
        })
        .await;

        assert_eq!(
            results.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    // Each shot keeps its own nonce and address through the whole journey,
    // because the confirmation step looks them up per wallet afterwards.
    #[tokio::test]
    async fn it_carries_each_wallet_nonce_through_to_the_result() {
        let results = fire_all_with((0..4).map(shot).collect(), |_: Shot| async {
            Ok("0x".to_owned())
        })
        .await;
        assert_eq!(
            results.iter().map(|r| r.nonce).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[tokio::test]
    async fn it_handles_an_empty_set_without_complaint() {
        let results = fire_all_with(Vec::new(), |_: Shot| async { Ok("0x".to_owned()) }).await;
        assert!(results.is_empty());
    }
}
