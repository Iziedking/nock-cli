//! Built ahead of its caller: the mint command is what finally reads a wallet
//! set, and until that lands the compiler is right that this is unused. Allowed
//! once here rather than per item, so it is one line to delete when the caller
//! arrives.
#![allow(dead_code)]
use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use thiserror::Error;
use zeroize::Zeroizing;

use super::keystore::Keystore;

/// The wallets one run drives.
///
/// An allowlist campaign is several wallets on one list, and running the binary
/// once per wallet means one passphrase prompt per wallet under time pressure,
/// which is how people end up leaving keys unlocked in a shell history. One run,
/// one prompt, N wallets.
///
/// ORDER IS LOAD BEARING. When a price rises mid-run the spend ceiling drops
/// wallets from the end of this list, so the order in the file is a decision the
/// user made in advance rather than one the tool makes under pressure. Nothing
/// here may sort, deduplicate into a set, or otherwise rearrange it.
#[derive(Debug, Error)]
pub enum SetError {
    #[error("the wallet set file is empty, so there is nobody to mint for")]
    Empty,
    #[error("{path} is listed twice, and one wallet cannot hold two places in a batch")]
    Duplicate { path: String },
    #[error("could not read the wallet set at {path}: {reason}")]
    Unreadable { path: String, reason: String },
    #[error("could not read the wallet at {path}: {reason}")]
    BadKeystore { path: String, reason: String },
    #[error("could not unlock {address}: {reason}")]
    WrongPassphrase { address: String, reason: String },
    #[error("the wallet at {path} holds an address that cannot be parsed")]
    BadAddress { path: String },
}

/// One unlocked wallet, and where it came from.
pub struct WalletEntry {
    /// Its place in the file, which is its place in the batch.
    pub index: usize,
    pub path: PathBuf,
    pub address: Address,
    pub secret: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for WalletEntry {
    /// Written by hand so a stray `{:?}` in a log or an error can never print a
    /// key. The derived one would.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletEntry")
            .field("index", &self.index)
            .field("path", &self.path)
            .field("address", &self.address)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub struct WalletSet {
    pub entries: Vec<WalletEntry>,
}

/// Reads the set file: one keystore path per line, `#` for comments.
///
/// Deliberately not a config format. A list of paths needs no schema, and a
/// plain list is something a person can write, read back and check against the
/// order they meant.
pub fn read_set_file(text: &str, base: &Path) -> Result<Vec<PathBuf>, SetError> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let candidate = Path::new(line);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base.join(candidate)
        };
        // The same keystore twice would double its share of the batch and be
        // rejected by the collection's own per-wallet cap at the chain, after
        // the slot was already spent.
        if out.contains(&path) {
            return Err(SetError::Duplicate {
                path: path.display().to_string(),
            });
        }
        out.push(path);
    }
    if out.is_empty() {
        return Err(SetError::Empty);
    }
    Ok(out)
}

/// Loads and decrypts every keystore in the set, in file order.
///
/// One passphrase for the whole set. A keystore that will not open fails the
/// whole unlock rather than being skipped: a partially unlocked set is not the
/// set the user asked for, and discovering that at T-0 is worse than not
/// starting.
pub fn unlock(paths: &[PathBuf], passphrase: &str) -> Result<WalletSet, SetError> {
    let mut entries = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let store = Keystore::load(path).map_err(|e| SetError::BadKeystore {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let address: Address = store.address().parse().map_err(|_| SetError::BadAddress {
            path: path.display().to_string(),
        })?;
        let secret = store
            .decrypt(passphrase)
            .map_err(|e| SetError::WrongPassphrase {
                address: store.address(),
                reason: e.to_string(),
            })?;
        entries.push(WalletEntry {
            index,
            path: path.clone(),
            address,
            secret,
        });
    }
    Ok(WalletSet { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from("keys")
    }

    // The spend ceiling drops from the end of this list, so the order is the
    // user's decision about who loses their place. Sorting it would quietly take
    // that decision away.
    #[test]
    fn it_keeps_the_order_the_file_gave() {
        let paths = read_set_file("c.json\na.json\nb.json\n", &base()).unwrap();
        assert_eq!(
            paths,
            vec![
                base().join("c.json"),
                base().join("a.json"),
                base().join("b.json"),
            ]
        );
    }

    #[test]
    fn it_ignores_blank_lines_and_comments() {
        let paths = read_set_file(
            "# the cold ones\n\na.json\n   \n# b is retired\nb.json\n",
            &base(),
        )
        .unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], base().join("a.json"));
    }

    #[test]
    fn it_trims_stray_whitespace_around_a_path() {
        let paths = read_set_file("  a.json  \n", &base()).unwrap();
        assert_eq!(paths, vec![base().join("a.json")]);
    }

    #[test]
    fn it_leaves_an_absolute_path_alone() {
        let line = if cfg!(windows) {
            "C:\\elsewhere\\x.json"
        } else {
            "/elsewhere/x.json"
        };
        let paths = read_set_file(&format!("{line}\n"), &base()).unwrap();
        assert_eq!(paths, vec![PathBuf::from(line)]);
    }

    // One wallet cannot hold two places in a batch. The collection's per-wallet
    // cap would reject the second mint after the slot was already spent, so this
    // is caught before anything is signed.
    #[test]
    fn it_refuses_the_same_keystore_listed_twice() {
        assert!(matches!(
            read_set_file("a.json\nb.json\na.json\n", &base()),
            Err(SetError::Duplicate { .. })
        ));
    }

    // A run with nobody in it should say so, not proceed to fire an empty batch.
    #[test]
    fn it_refuses_an_empty_set_rather_than_running_with_nobody() {
        assert!(matches!(read_set_file("", &base()), Err(SetError::Empty)));
        assert!(matches!(
            read_set_file("\n# nothing but a note\n\n", &base()),
            Err(SetError::Empty)
        ));
    }

    // A key must never reach a log through a careless debug print.
    #[test]
    fn it_never_prints_the_key() {
        let entry = WalletEntry {
            index: 0,
            path: PathBuf::from("a.json"),
            address: Address::ZERO,
            secret: Zeroizing::new([0xabu8; 32]),
        };
        let printed = format!("{entry:?}");
        assert!(printed.contains("<redacted>"));
        assert!(!printed.contains("ab"), "the key bytes leaked into Debug");
    }
}
