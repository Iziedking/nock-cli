use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zeroize::Zeroizing;

use crate::wallet::keystore::{generate, Keystore, KeystoreError};

/// Where a wallet lives unless told otherwise. Beside the binary rather than in
/// a config directory, so a user can see the file they are responsible for.
pub const DEFAULT_PATH: &str = "nock-wallet.json";

pub fn default_path() -> PathBuf {
    PathBuf::from(DEFAULT_PATH)
}

/// Creates a new wallet, encrypted, and prints only its address.
pub fn new_wallet(path: &Path) -> ExitCode {
    if path.exists() {
        eprintln!(
            "A wallet already exists at {}.\n\
             Refusing to overwrite it: that would destroy the key with no way back.\n\
             Move it aside first, or pass a different path.",
            path.display()
        );
        return ExitCode::FAILURE;
    }

    let passphrase = match read_new_passphrase() {
        Ok(p) => p,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let secret = generate();
    println!("\nEncrypting. This takes a moment on purpose.");

    let store = match Keystore::encrypt(&secret, &passphrase) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("could not create the wallet: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = store.save(path) {
        eprintln!("could not save the wallet: {err}");
        return ExitCode::FAILURE;
    }

    println!("\n  address  {}", store.address());
    println!("  file     {}", path.display());
    println!(
        "\n  This file is the only copy of that key and the passphrase is the only\n  \
         way into it. Neither can be recovered. Back the file up somewhere the\n  \
         passphrase is not written down.\n\n  \
         It is a standard v3 keystore, so geth and MetaMask can import it if you\n  \
         ever want to stop using this tool.\n"
    );
    ExitCode::SUCCESS
}

/// Prints the address a wallet holds, without unlocking it.
pub fn show(path: &Path) -> ExitCode {
    match Keystore::load(path) {
        Ok(store) => {
            println!("\n  address  {}", store.address());
            println!("  file     {}", path.display());
            println!(
                "  cipher   {} / {}\n",
                store.crypto.kdf, store.crypto.cipher
            );
            ExitCode::SUCCESS
        }
        Err(KeystoreError::Io(_)) => {
            eprintln!(
                "No wallet at {}. Create one with `nock wallets new`.",
                path.display()
            );
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("could not read the wallet: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Unlocks a wallet and confirms the passphrase works, printing nothing secret.
pub fn unlock(path: &Path) -> ExitCode {
    let store = match Keystore::load(path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("could not read the wallet: {err}");
            return ExitCode::FAILURE;
        }
    };

    let Ok(passphrase) = read_secret("Passphrase: ") else {
        eprintln!("could not read the passphrase");
        return ExitCode::FAILURE;
    };

    match store.decrypt(&passphrase) {
        Ok(_secret) => {
            // The key is dropped, and wiped, at the end of this scope. Nothing
            // about it is printed: the address already came from the file.
            println!("\n  Unlocked. {}\n", store.address());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn read_new_passphrase() -> Result<Zeroizing<String>, String> {
    println!(
        "\nChoose a passphrase for this wallet.\n\
         It is never stored, never sent anywhere, and cannot be recovered.\n"
    );
    let first = read_secret("Passphrase: ")?;

    if first.chars().count() < 8 {
        return Err("Too short. Use at least eight characters.".to_owned());
    }

    let again = read_secret("Again: ")?;

    if *first != *again {
        // Confirmed before anything is written. A mistyped passphrase on a
        // wallet that has already been saved is an unrecoverable key.
        return Err("Those do not match. Nothing was written.".to_owned());
    }
    Ok(first)
}

/// Reads a passphrase without echoing it.
///
/// When stdin is a terminal this uses the terminal directly, so the passphrase
/// never appears on screen and never reaches a shell history. When stdin is a
/// pipe it reads a line instead, which is what makes the tool scriptable and
/// testable. Both paths keep the value inside `Zeroizing`.
fn read_secret(prompt: &str) -> Result<Zeroizing<String>, String> {
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(prompt)
            .map(Zeroizing::new)
            .map_err(|_| "could not read the passphrase".to_owned());
    }
    let mut line = Zeroizing::new(String::new());
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|_| "could not read the passphrase".to_owned())?;
    Ok(Zeroizing::new(line.trim_end().to_owned()))
}
