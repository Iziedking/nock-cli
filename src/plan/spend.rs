use thiserror::Error;

/// A ceiling on what one run may spend on mint prices, across every stage it
/// walks.
///
/// Not per stage and not per wallet. The re-plan loop can meet a price that
/// rose after the user typed the number, and there is nobody to ask mid-run, so
/// `--fire` stops being sufficient authorization the moment money is involved.
/// One number, for the whole run, decided in advance.
#[derive(Debug, Clone)]
pub struct SpendCeiling {
    limit_wei: u128,
    committed_wei: u128,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SpendError {
    #[error(
        "refusing to commit {attempted_wei} wei, only {remaining_wei} wei of --max-spend is left"
    )]
    Exceeded {
        attempted_wei: u128,
        remaining_wei: u128,
    },
}

/// How many wallets a stage could afford, and how many it could not.
///
/// Used by the wallet-set path, which is the next piece of work. Kept here with
/// its tests because those tests are the specification for dropping from the end
/// of the set, and writing them once is cheaper than agreeing it twice.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub struct Admitted {
    pub taken: usize,
    pub dropped: usize,
    pub committed_wei: u128,
}

impl SpendCeiling {
    pub const fn new(limit_wei: u128) -> Self {
        Self {
            limit_wei,
            committed_wei: 0,
        }
    }

    pub const fn remaining(&self) -> u128 {
        self.limit_wei.saturating_sub(self.committed_wei)
    }

    pub const fn fits(&self, cost_wei: u128) -> bool {
        cost_wei <= self.remaining()
    }

    pub fn commit(&mut self, cost_wei: u128) -> Result<(), SpendError> {
        if !self.fits(cost_wei) {
            return Err(SpendError::Exceeded {
                attempted_wei: cost_wei,
                remaining_wei: self.remaining(),
            });
        }
        self.committed_wei += cost_wei;
        Ok(())
    }

    /// Takes wallets in set order until the money runs out.
    ///
    /// Dropping from the end means which wallet loses its place was decided by
    /// the user when they wrote the set file, rather than by the tool under time
    /// pressure. A free stage admits everybody: there is nothing to ration.
    #[allow(dead_code)]
    pub fn admit(&mut self, unit_cost_wei: u128, wallets: usize) -> Admitted {
        if unit_cost_wei == 0 {
            return Admitted {
                taken: wallets,
                dropped: 0,
                committed_wei: 0,
            };
        }
        let affordable = (self.remaining() / unit_cost_wei) as usize;
        let taken = affordable.min(wallets);
        let committed_wei = unit_cost_wei * taken as u128;
        self.committed_wei += committed_wei;
        Admitted {
            taken,
            dropped: wallets - taken,
            committed_wei,
        }
    }
}

/// Turns "0.05" into wei.
///
/// Written here rather than pulled in, because the only place the CLI takes an
/// amount from a human is --max-spend, and a rounding surprise in that number is
/// a rounding surprise in what somebody is authorising. Rejects anything it
/// cannot represent exactly instead of truncating quietly.
pub fn parse_eth(input: &str) -> Result<u128, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("expected an amount in ETH, for example 0.05".to_owned());
    }
    let (whole, frac) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text, ""),
    };
    if frac.len() > 18 {
        return Err(format!(
            "{text} has more than 18 decimal places, which is finer than wei"
        ));
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("{text} is not a number of ETH"));
    }
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| format!("{text} is too large"))?
    };
    let padded = format!("{frac:0<18}");
    let frac: u128 = if padded.is_empty() {
        0
    } else {
        padded.parse().map_err(|_| format!("{text} is too large"))?
    };
    whole
        .checked_mul(1_000_000_000_000_000_000)
        .and_then(|w| w.checked_add(frac))
        .ok_or_else(|| format!("{text} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETH: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn it_admits_every_wallet_when_the_run_can_afford_them() {
        let mut c = SpendCeiling::new(ETH);
        let a = c.admit(ETH / 10, 5);
        assert_eq!(a.taken, 5);
        assert_eq!(a.dropped, 0);
        assert_eq!(c.remaining(), ETH / 2);
    }

    // Set order is the user's decision, made when they wrote the file, so the
    // tail is what goes rather than something the tool picks at T-2 minutes.
    #[test]
    fn it_drops_from_the_end_rather_than_refusing_the_whole_stage() {
        let mut c = SpendCeiling::new(ETH / 4);
        let a = c.admit(ETH / 10, 5);
        assert_eq!(a.taken, 2);
        assert_eq!(a.dropped, 3);
    }

    // The ceiling is the run, not the stage. A later stage is planned against
    // what is left, because the re-plan loop can meet a price that rose after
    // the user typed the number and there is nobody to ask mid-run.
    #[test]
    fn it_accumulates_across_stages() {
        let mut c = SpendCeiling::new(ETH);
        c.admit(ETH / 2, 1);
        let a = c.admit(ETH / 2, 2);
        assert_eq!(a.taken, 1);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn it_refuses_a_single_cost_over_what_is_left() {
        let mut c = SpendCeiling::new(100);
        assert!(matches!(
            c.commit(101),
            Err(SpendError::Exceeded {
                attempted_wei: 101,
                remaining_wei: 100
            })
        ));
        assert_eq!(
            c.remaining(),
            100,
            "a refused commit must not consume headroom"
        );
    }

    // A free stage imposes no ceiling, so it must not need one.
    #[test]
    fn it_admits_everyone_at_zero_cost_even_with_no_headroom() {
        let mut c = SpendCeiling::new(0);
        assert_eq!(c.admit(0, 9).taken, 9);
    }

    #[test]
    fn it_parses_a_whole_number_of_eth() {
        assert_eq!(parse_eth("1").unwrap(), ETH);
        assert_eq!(parse_eth(" 2 ").unwrap(), 2 * ETH);
    }

    #[test]
    fn it_parses_a_fraction_without_losing_precision() {
        assert_eq!(parse_eth("0.05").unwrap(), ETH / 20);
        assert_eq!(parse_eth("0.000000000000000001").unwrap(), 1);
    }

    // Silently truncating what somebody typed is the wrong answer for a number
    // that authorises spending.
    #[test]
    fn it_refuses_more_precision_than_wei() {
        assert!(parse_eth("0.0000000000000000001").is_err());
    }

    #[test]
    fn it_refuses_something_that_is_not_a_number() {
        assert!(parse_eth("lots").is_err());
        assert!(parse_eth("").is_err());
        assert!(parse_eth("1.2.3").is_err());
    }

    #[test]
    fn it_reports_what_one_stage_actually_committed() {
        let mut c = SpendCeiling::new(ETH);
        assert_eq!(c.admit(ETH / 4, 3).committed_wei, (ETH / 4) * 3);
    }
}
