use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use aes::cipher::{KeyIvInit, StreamCipher};
use alloy_primitives::{keccak256, Address};
use k256::ecdsa::SigningKey;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Web3 Secret Storage v3, the format `geth` and `MetaMask` both read.
///
/// Deliberately the standard rather than something of our own. A user who stops
/// trusting Nock should be able to import this file anywhere and walk away, the
/// same reason the minting accounts answer to their owner and a partner
/// permission is revocable in one call. Nothing here should strand anybody.
///
/// The prior art in this space keeps private keys as plaintext JSON and hardens
/// file permissions only on Unix, leaving Windows with nothing. Encrypting is
/// the answer to both: a stolen file is useless without the passphrase, and the
/// permissions become defence in depth rather than the only defence.
const SCRYPT_LOG_N: u8 = 18;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const DK_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("this keystore is version {0}, and only version 3 is supported")]
    UnsupportedVersion(u32),
    #[error("this keystore uses {0}, which is not supported")]
    UnsupportedCipher(String),
    #[error("the keystore file is not valid JSON: {0}")]
    Malformed(String),
    #[error("a wallet already exists at {0}; choose another path rather than overwriting a key")]
    AlreadyExists(PathBuf),
    #[error("could not read or write the keystore: {0}")]
    Io(String),
    #[error("the decrypted material is not a valid signing key")]
    NotAKey,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Keystore {
    pub version: u32,
    pub id: String,
    /// Lowercase, without the `0x` prefix, as the standard has it.
    pub address: String,
    pub crypto: Crypto,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Crypto {
    pub cipher: String,
    pub ciphertext: String,
    pub cipherparams: CipherParams,
    pub kdf: String,
    pub kdfparams: KdfParams,
    pub mac: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CipherParams {
    pub iv: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KdfParams {
    pub dklen: usize,
    pub n: u32,
    pub p: u32,
    pub r: u32,
    pub salt: String,
}

fn derive(
    passphrase: &str,
    salt: &[u8],
    params: &KdfParams,
) -> Result<Zeroizing<Vec<u8>>, KeystoreError> {
    let log_n =
        u8::try_from(params.n.trailing_zeros()).map_err(|_| KeystoreError::WrongPassphrase)?;
    let scrypt_params = scrypt::Params::new(log_n, params.r, params.p, params.dklen)
        .map_err(|_| KeystoreError::WrongPassphrase)?;
    let mut derived = Zeroizing::new(vec![0u8; params.dklen]);
    scrypt::scrypt(passphrase.as_bytes(), salt, &scrypt_params, &mut derived)
        .map_err(|_| KeystoreError::WrongPassphrase)?;
    Ok(derived)
}

impl Keystore {
    /// Encrypts a signing key under a passphrase.
    pub fn encrypt(secret: &[u8; 32], passphrase: &str) -> Result<Self, KeystoreError> {
        let signing_key = SigningKey::from_slice(secret).map_err(|_| KeystoreError::NotAKey)?;
        let address = Address::from_private_key(&signing_key);

        let mut salt = [0u8; 32];
        let mut iv = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut iv);

        let params = KdfParams {
            dklen: DK_LEN,
            n: 1 << SCRYPT_LOG_N,
            p: SCRYPT_P,
            r: SCRYPT_R,
            salt: hex::encode(salt),
        };
        let derived = derive(passphrase, &salt, &params)?;

        // The plaintext must not outlive this function in an unwiped buffer.
        let mut ciphertext = Zeroizing::new(secret.to_vec());
        Aes128Ctr::new(derived[..16].into(), (&iv).into()).apply_keystream(&mut ciphertext);

        // MAC over the second half of the derived key and the ciphertext. This
        // is what makes a wrong passphrase a clean error rather than garbage
        // that looks like a key.
        let mac = keccak256([&derived[16..32], &ciphertext[..]].concat());

        Ok(Self {
            version: 3,
            id: uuid::Uuid::new_v4().to_string(),
            address: hex::encode(address.as_slice()),
            crypto: Crypto {
                cipher: "aes-128-ctr".to_owned(),
                ciphertext: hex::encode(&ciphertext[..]),
                cipherparams: CipherParams {
                    iv: hex::encode(iv),
                },
                kdf: "scrypt".to_owned(),
                kdfparams: params,
                mac: hex::encode(mac),
            },
        })
    }

    pub fn decrypt(&self, passphrase: &str) -> Result<Zeroizing<[u8; 32]>, KeystoreError> {
        if self.version != 3 {
            return Err(KeystoreError::UnsupportedVersion(self.version));
        }
        if self.crypto.cipher != "aes-128-ctr" {
            return Err(KeystoreError::UnsupportedCipher(self.crypto.cipher.clone()));
        }

        let salt = hex::decode(&self.crypto.kdfparams.salt)
            .map_err(|_| KeystoreError::Malformed("salt".to_owned()))?;
        let iv = hex::decode(&self.crypto.cipherparams.iv)
            .map_err(|_| KeystoreError::Malformed("iv".to_owned()))?;
        let ciphertext = Zeroizing::new(
            hex::decode(&self.crypto.ciphertext)
                .map_err(|_| KeystoreError::Malformed("ciphertext".to_owned()))?,
        );

        let derived = derive(passphrase, &salt, &self.crypto.kdfparams)?;

        // Verified before decrypting. A wrong passphrase must be a clear
        // refusal, never 32 bytes of noise handed back as if it were a key.
        let expected = hex::decode(&self.crypto.mac)
            .map_err(|_| KeystoreError::Malformed("mac".to_owned()))?;
        let actual = keccak256([&derived[16..32], &ciphertext[..]].concat());
        if actual.as_slice() != expected.as_slice() {
            return Err(KeystoreError::WrongPassphrase);
        }

        let mut plain = Zeroizing::new(ciphertext.to_vec());
        if iv.len() != 16 {
            return Err(KeystoreError::Malformed("iv length".to_owned()));
        }
        Aes128Ctr::new(derived[..16].into(), iv.as_slice().into()).apply_keystream(&mut plain);

        let bytes: [u8; 32] = plain[..].try_into().map_err(|_| KeystoreError::NotAKey)?;
        SigningKey::from_slice(&bytes).map_err(|_| KeystoreError::NotAKey)?;
        Ok(Zeroizing::new(bytes))
    }

    /// Writes the keystore, refusing to overwrite an existing one.
    ///
    /// A wallet file is not a build artefact. Clobbering one destroys a key
    /// with no way back, so this fails rather than asks.
    pub fn save(&self, path: &Path) -> Result<(), KeystoreError> {
        let encoded =
            serde_json::to_vec_pretty(self).map_err(|e| KeystoreError::Malformed(e.to_string()))?;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        restrict(&mut options);

        let mut file = options.open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                KeystoreError::AlreadyExists(path.to_path_buf())
            } else {
                KeystoreError::Io(e.to_string())
            }
        })?;
        file.write_all(&encoded)
            .map_err(|e| KeystoreError::Io(e.to_string()))?;
        file.write_all(b"\n")
            .map_err(|e| KeystoreError::Io(e.to_string()))?;
        // On disk before we claim it is saved. A user who is told their wallet
        // exists and then loses power must still have it.
        file.sync_all()
            .map_err(|e| KeystoreError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, KeystoreError> {
        let text = std::fs::read_to_string(path).map_err(|e| KeystoreError::Io(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| KeystoreError::Malformed(e.to_string()))
    }

    #[must_use]
    pub fn address(&self) -> String {
        format!("0x{}", self.address)
    }
}

#[cfg(unix)]
fn restrict(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

// Windows has no mode bits, and this is exactly where the prior art leaves keys
// unprotected. The file is encrypted, so permissions are defence in depth here
// rather than the only defence, which is the whole reason encrypting was worth
// doing instead of hardening a plaintext file.
#[cfg(not(unix))]
const fn restrict(_options: &mut OpenOptions) {}

/// A fresh key from the OS CSPRNG.
#[must_use]
pub fn generate() -> Zeroizing<[u8; 32]> {
    let signing_key = SigningKey::random(&mut OsRng);
    Zeroizing::new(signing_key.to_bytes().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [
        0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff,
        0x94, 0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2,
        0xff, 0x80,
    ];

    /// The real parameters take a second or two by design. Tests use a light
    /// KDF so the suite stays quick; everything else is identical.
    fn light(store: &mut Keystore) {
        store.crypto.kdfparams.n = 1 << 12;
    }

    fn encrypt_light(secret: &[u8; 32], pass: &str) -> Keystore {
        // Encrypt with light params directly, so the MAC matches them.
        let mut salt = [0u8; 32];
        let mut iv = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut iv);
        let params = KdfParams {
            dklen: DK_LEN,
            n: 1 << 12,
            p: 1,
            r: 8,
            salt: hex::encode(salt),
        };
        let derived = derive(pass, &salt, &params).unwrap();
        let mut ciphertext = secret.to_vec();
        Aes128Ctr::new(derived[..16].into(), (&iv).into()).apply_keystream(&mut ciphertext);
        let mac = keccak256([&derived[16..32], &ciphertext[..]].concat());
        let signing_key = SigningKey::from_slice(secret).unwrap();
        Keystore {
            version: 3,
            id: uuid::Uuid::new_v4().to_string(),
            address: hex::encode(Address::from_private_key(&signing_key).as_slice()),
            crypto: Crypto {
                cipher: "aes-128-ctr".to_owned(),
                ciphertext: hex::encode(&ciphertext),
                cipherparams: CipherParams {
                    iv: hex::encode(iv),
                },
                kdf: "scrypt".to_owned(),
                kdfparams: params,
                mac: hex::encode(mac),
            },
        }
    }

    #[test]
    fn a_key_survives_a_round_trip() {
        let store = encrypt_light(&SECRET, "correct horse battery staple");
        let out = store.decrypt("correct horse battery staple").unwrap();
        assert_eq!(&out[..], &SECRET[..]);
    }

    #[test]
    fn the_address_matches_the_key_inside() {
        let store = encrypt_light(&SECRET, "pw");
        assert_eq!(
            store.address(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    /// The point of the whole exercise: the file must not contain the key.
    #[test]
    fn the_stored_file_never_contains_the_plaintext_key() {
        let store = encrypt_light(&SECRET, "pw");
        let json = serde_json::to_string(&store).unwrap();
        assert!(
            !json.contains(&hex::encode(SECRET)),
            "the key is sitting in the file"
        );
        assert!(!format!("{store:?}").contains(&hex::encode(SECRET)));
    }

    /// A wrong passphrase must be a clean refusal, never 32 bytes of noise
    /// handed back as though it were a key.
    #[test]
    fn a_wrong_passphrase_is_refused_rather_than_returning_noise() {
        let store = encrypt_light(&SECRET, "right");
        assert!(matches!(
            store.decrypt("wrong"),
            Err(KeystoreError::WrongPassphrase)
        ));
    }

    #[test]
    fn tampering_with_the_ciphertext_is_caught() {
        let mut store = encrypt_light(&SECRET, "pw");
        let mut bytes = hex::decode(&store.crypto.ciphertext).unwrap();
        bytes[0] ^= 0xff;
        store.crypto.ciphertext = hex::encode(bytes);
        assert!(matches!(
            store.decrypt("pw"),
            Err(KeystoreError::WrongPassphrase)
        ));
    }

    #[test]
    fn refuses_a_format_it_does_not_understand() {
        let mut store = encrypt_light(&SECRET, "pw");
        store.version = 4;
        assert!(matches!(
            store.decrypt("pw"),
            Err(KeystoreError::UnsupportedVersion(4))
        ));
        let mut other = encrypt_light(&SECRET, "pw");
        other.crypto.cipher = "aes-256-gcm".to_owned();
        assert!(matches!(
            other.decrypt("pw"),
            Err(KeystoreError::UnsupportedCipher(_))
        ));
    }

    /// Interoperability is the reason this is v3 and not our own format. A user
    /// who stops trusting Nock should be able to take this to geth and leave.
    #[test]
    fn the_json_is_shaped_the_way_geth_expects() {
        let store = encrypt_light(&SECRET, "pw");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(v["version"], 3);
        assert_eq!(v["crypto"]["cipher"], "aes-128-ctr");
        assert_eq!(v["crypto"]["kdf"], "scrypt");
        for key in ["ciphertext", "mac", "cipherparams", "kdfparams"] {
            assert!(v["crypto"].get(key).is_some(), "missing crypto.{key}");
        }
        for key in ["dklen", "n", "p", "r", "salt"] {
            assert!(
                v["crypto"]["kdfparams"].get(key).is_some(),
                "missing kdfparams.{key}"
            );
        }
        // No 0x prefix on the address, which is what the standard says.
        assert!(!v["address"].as_str().unwrap().starts_with("0x"));
    }

    #[test]
    fn two_keystores_of_the_same_key_differ() {
        let a = encrypt_light(&SECRET, "pw");
        let b = encrypt_light(&SECRET, "pw");
        assert_ne!(
            a.crypto.ciphertext, b.crypto.ciphertext,
            "salt or iv is not random"
        );
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn generated_keys_are_distinct_and_valid() {
        let a = generate();
        let b = generate();
        assert_ne!(&a[..], &b[..]);
        assert!(SigningKey::from_slice(&a[..]).is_ok());
    }

    /// A wallet file is not a build artefact. Clobbering one destroys a key
    /// with no way back.
    #[test]
    fn refuses_to_overwrite_an_existing_wallet() {
        let dir = std::env::temp_dir().join(format!("nock-ks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wallet.json");
        let store = encrypt_light(&SECRET, "pw");

        store.save(&path).unwrap();
        assert!(matches!(
            store.save(&path),
            Err(KeystoreError::AlreadyExists(_))
        ));

        let loaded = Keystore::load(&path).unwrap();
        assert_eq!(&loaded.decrypt("pw").unwrap()[..], &SECRET[..]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_parameters_still_round_trip() {
        // Slow on purpose: this is the only test that exercises the shipped KDF.
        let store = Keystore::encrypt(&SECRET, "pw").unwrap();
        assert_eq!(store.crypto.kdfparams.n, 1 << SCRYPT_LOG_N);
        assert_eq!(&store.decrypt("pw").unwrap()[..], &SECRET[..]);
        let mut light_store = Keystore::encrypt(&SECRET, "pw").unwrap();
        light(&mut light_store);
        assert_eq!(light_store.crypto.kdfparams.n, 1 << 12);
    }
}
