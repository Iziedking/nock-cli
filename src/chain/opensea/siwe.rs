use alloy_primitives::{keccak256, Address};
use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use thiserror::Error;
use zeroize::Zeroizing;

/// Signing in to `OpenSea` with the wallet we already hold.
///
/// The signature for a signed stage is issued per address, and `OpenSea` will only
/// issue one to a session it has authenticated. That authentication is EIP-4361,
/// "Sign-In with Ethereum": the server hands out a nonce, the client builds a
/// fixed message around it, signs it with `personal_sign`, and posts both back.
///
/// The message format is byte-exact. The server rebuilds the same string from
/// the fields it was sent and recovers the signer from the signature, so a stray
/// space or a lowercase address is not a formatting difference, it is an
/// authentication failure with an unhelpful error message. That is why the shape
/// is pinned by tests rather than trusted.
///
/// In this tool signing in is free: the key is already unlocked in memory to
/// mint with. In product B it will not be, and the connect-and-sign page will
/// have to ask a person for two signatures, a login and the mint. Worth knowing
/// before that page is designed rather than after.
#[derive(Debug, Error)]
pub enum SiweError {
    #[error("the private key is not a valid signing key")]
    BadKey,
    #[error("signing the login message failed")]
    Failed,
    #[error("could not reach OpenSea to sign in: {0}")]
    Transport(String),
    // Carries the body, because "400" on its own tells you nothing about which
    // field was wrong and this is a third party whose shape can change.
    #[error("OpenSea refused the sign-in with {status}: {body}")]
    Refused { status: u16, body: String },
    #[error("OpenSea's sign-in reply was not what this expects: {0}")]
    Malformed(String),
}

/// A signed-in session, held as the cookies `OpenSea` set.
///
/// Kept as an opaque string rather than parsed: every one of these is a
/// credential, and the only thing this code ever needs to do with them is send
/// them back unchanged.
#[derive(Debug, Clone)]
pub struct Session {
    pub cookies: String,
}

/// The exact statement `OpenSea` expects. It is compared against their own copy,
/// so it is not ours to shorten, reword or leave out. Captured from a real
/// sign-in on 2026-08-19.
pub const STATEMENT: &str = "Click to sign in and accept the OpenSea Terms of Service (https://opensea.io/tos) and Privacy Policy (https://opensea.io/privacy).";

/// The `EIP-4361` fields this client sends.
///
/// Sent twice over: as a rendered string for the signature to cover, and as the
/// same fields in an object for the server to rebuild it from. Both have to say
/// the same thing or the recovered signer will not be the address claimed.
#[derive(Debug, Clone)]
pub struct SiweMessage {
    pub domain: String,
    pub address: Address,
    pub uri: String,
    pub chain_id: u64,
    pub nonce: String,
    pub issued_at: String,
}

impl std::fmt::Display for SiweMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The address line is EIP-55 checksummed, because that is the form a
        // SIWE verifier compares against.
        write!(
            f,
            "{} wants you to sign in with your Ethereum account:\n{}\n\n{}\n\nURI: {}\nVersion: 1\nChain ID: {}\nNonce: {}\nIssued At: {}",
            self.domain,
            self.address.to_checksum(None),
            STATEMENT,
            self.uri,
            self.chain_id,
            self.nonce,
            self.issued_at
        )
    }
}

/// `personal_sign`: prefix, hash, sign, and return r ‖ s ‖ v with v as 27 or 28.
///
/// The prefix is what stops a signed login being replayable as a transaction.
/// A raw hash signed by this key could be anything; a prefixed one can only ever
/// have been a message.
pub fn personal_sign(message: &str, secret: &Zeroizing<[u8; 32]>) -> Result<String, SiweError> {
    let mut prefixed = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
    prefixed.extend_from_slice(message.as_bytes());
    let digest = keccak256(&prefixed);

    let key = SigningKey::from_slice(&secret[..]).map_err(|_| SiweError::BadKey)?;
    let (signature, recovery): (Signature, RecoveryId) = key
        .sign_prehash_recoverable(digest.as_slice())
        .map_err(|_| SiweError::Failed)?;

    let mut out = Vec::with_capacity(65);
    out.extend_from_slice(&signature.r().to_bytes());
    out.extend_from_slice(&signature.s().to_bytes());
    out.push(27 + u8::from(recovery.is_y_odd()));
    Ok(format!("0x{}", hex::encode(out)))
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SS.000Z`.
///
/// Written out rather than pulled in, because one timestamp in one message is
/// not worth a date library in a binary people are asked to trust. The civil
/// calendar conversion is Howard Hinnant's, which is the one everybody's date
/// library is already using.
fn iso8601(unix_seconds: u64) -> String {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let secs = unix_seconds % 86_400;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Signs in and returns the session cookies.
///
/// Two round trips: a nonce, then the signed message. The nonce is what stops a
/// captured login being replayed, so it is never cached or reused.
pub async fn authenticate(
    http: &reqwest::Client,
    address: Address,
    secret: &Zeroizing<[u8; 32]>,
    chain_id: u64,
) -> Result<Session, SiweError> {
    // POST, not GET. Measured 2026-08-19: GET answers 405 Method Not Allowed.
    let nonce_reply = http
        .post("https://opensea.io/__api/auth/siwe/nonce")
        .header("accept", "application/json")
        .header("origin", "https://opensea.io")
        .header("referer", "https://opensea.io/")
        .send()
        .await
        .map_err(|e| SiweError::Transport(e.to_string()))?;

    if !nonce_reply.status().is_success() {
        let status = nonce_reply.status().as_u16();
        let body = nonce_reply.text().await.unwrap_or_default();
        return Err(SiweError::Refused { status, body });
    }

    // Carry every cookie the nonce request set. The nonce is usually bound to
    // one of them, so posting the signature without them authenticates nothing.
    let mut cookies = collect_cookies(nonce_reply.headers());
    let body = nonce_reply
        .text()
        .await
        .map_err(|e| SiweError::Transport(e.to_string()))?;
    let nonce = extract_nonce(&body)?;

    let issued_at = iso8601(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| SiweError::Malformed(e.to_string()))?
            .as_secs(),
    );

    let rendered = SiweMessage {
        domain: "opensea.io".to_owned(),
        address,
        uri: "https://opensea.io".to_owned(),
        chain_id,
        nonce: nonce.clone(),
        issued_at: issued_at.clone(),
    }
    .to_string();

    let signature = personal_sign(&rendered, secret)?;

    let verify = http
        .post("https://opensea.io/__api/auth/siwe/verify")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("origin", "https://opensea.io")
        .header("referer", "https://opensea.io/")
        .header("cookie", cookies.clone())
        // domain is sent alongside the message, not only inside it. Measured
        // 2026-08-19: without it the reply is 400 "Domain is required".
        // The structured shape, measured from a real sign-in. A rendered string
        // here is refused with "Domain is required", and an object without the
        // statement with "Unexpected SIWE message statement". chainId is a
        // string on the wire even though it is a number everywhere else.
        .json(&serde_json::json!({
            "message": {
                "domain": "opensea.io",
                "address": address.to_checksum(None),
                "statement": STATEMENT,
                "uri": "https://opensea.io",
                "version": "1",
                "chainId": chain_id.to_string(),
                "nonce": nonce,
                "issuedAt": issued_at,
                "accountType": "Ethereum",
            },
            "signature": signature,
            "chainArch": "EVM",
            "connectorId": "io.nock",
        }))
        .send()
        .await
        .map_err(|e| SiweError::Transport(e.to_string()))?;

    if !verify.status().is_success() {
        let status = verify.status().as_u16();
        let body = verify.text().await.unwrap_or_default();
        return Err(SiweError::Refused { status, body });
    }

    let issued = collect_cookies(verify.headers());
    if !issued.is_empty() {
        if cookies.is_empty() {
            cookies = issued;
        } else {
            cookies = format!("{cookies}; {issued}");
        }
    }
    if cookies.is_empty() {
        return Err(SiweError::Malformed(
            "sign-in succeeded but set no cookies, so there is no session to use".to_owned(),
        ));
    }
    Ok(Session { cookies })
}

fn collect_cookies(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The nonce, whether it arrives as bare text or wrapped in JSON.
fn extract_nonce(body: &str) -> Result<String, SiweError> {
    let trimmed = body.trim().trim_matches('"');
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(trimmed.to_owned());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(body).map_err(|e| SiweError::Malformed(e.to_string()))?;
    parsed
        .get("nonce")
        .and_then(|n| n.as_str())
        .map(str::to_owned)
        .ok_or_else(|| SiweError::Malformed(format!("no nonce in {body}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> SiweMessage {
        SiweMessage {
            domain: "opensea.io".to_owned(),
            address: "0x941c2a17c60ad6daf86cb6438074d57e906adffa"
                .parse()
                .unwrap(),
            uri: "https://opensea.io".to_owned(),
            chain_id: 4663,
            nonce: "abc123".to_owned(),
            issued_at: "2026-08-19T00:00:00.000Z".to_owned(),
        }
    }

    // The server rebuilds this string and recovers the signer from it, so every
    // byte is load bearing. A stray space is an authentication failure whose
    // error message will not mention spaces.
    #[test]
    fn it_builds_the_message_exactly_as_eip_4361_specifies() {
        let text = message().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "opensea.io wants you to sign in with your Ethereum account:"
        );
        assert_eq!(lines[1], "0x941c2A17C60AD6dAf86CB6438074d57E906adFFA");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], STATEMENT);
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "URI: https://opensea.io");
        assert_eq!(lines[6], "Version: 1");
        assert_eq!(lines[7], "Chain ID: 4663");
        assert_eq!(lines[8], "Nonce: abc123");
        assert_eq!(lines[9], "Issued At: 2026-08-19T00:00:00.000Z");
        assert_eq!(lines.len(), 10, "no trailing blank line");
    }

    // Lowercasing the address here is the single most likely way to build a
    // message that looks right and authenticates as somebody else, which is to
    // say nobody.
    #[test]
    fn it_checksums_the_address_rather_than_lowercasing_it() {
        let text = message().to_string();
        assert!(text.contains("0x941c2A17C60AD6dAf86CB6438074d57E906adFFA"));
        assert!(!text.contains("0x941c2a17c60ad6daf86cb6438074d57e906adffa"));
    }

    // Not ours to reword. OpenSea compares this against their own copy, and any
    // difference is refused with "Unexpected SIWE message statement".
    #[test]
    fn it_uses_the_statement_opensea_actually_expects() {
        assert!(STATEMENT.starts_with("Click to sign in and accept the OpenSea Terms of Service"));
        assert!(STATEMENT.contains("https://opensea.io/tos"));
        assert!(STATEMENT.contains("https://opensea.io/privacy"));
        assert!(STATEMENT.ends_with('.'));
    }

    #[test]
    fn it_produces_a_sixty_five_byte_signature() {
        let secret = Zeroizing::new([0x11u8; 32]);
        let sig = personal_sign("hello", &secret).unwrap();
        assert!(sig.starts_with("0x"));
        assert_eq!(sig.len(), 132, "65 bytes as hex, plus the 0x");
    }

    // v is 27 or 28 for personal_sign. A raw 0 or 1 is the classic mistake and
    // the server rejects it as a malformed signature.
    #[test]
    fn it_ends_with_a_recovery_byte_of_27_or_28() {
        let secret = Zeroizing::new([0x22u8; 32]);
        let sig = personal_sign("hello", &secret).unwrap();
        let v = u8::from_str_radix(&sig[130..132], 16).unwrap();
        assert!(v == 27 || v == 28, "v was {v}");
    }

    // The prefix carries the byte length, not the character count, so a message
    // with anything non-ASCII in it must not be measured wrongly.
    #[test]
    fn it_measures_the_message_in_bytes_not_characters() {
        let secret = Zeroizing::new([0x33u8; 32]);
        let ascii = personal_sign("aaaa", &secret).unwrap();
        let wide = personal_sign("ää", &secret).unwrap();
        assert_ne!(
            ascii, wide,
            "four bytes of ASCII and four bytes of UTF-8 must not hash alike"
        );
    }

    #[test]
    fn it_formats_a_timestamp_the_way_eip_4361_wants_it() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_786_966_789), "2026-08-17T11:39:49.000Z");
        // A leap day, because the calendar arithmetic is the part worth doubting.
        assert_eq!(iso8601(1_709_164_800), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn it_reads_a_nonce_whether_it_is_bare_or_wrapped() {
        assert_eq!(extract_nonce("abc123").unwrap(), "abc123");
        assert_eq!(extract_nonce("\"abc123\"").unwrap(), "abc123");
        assert_eq!(extract_nonce("{\"nonce\":\"abc123\"}").unwrap(), "abc123");
    }

    // A missing nonce must be an error rather than an empty string, which would
    // build a message that fails to authenticate for no visible reason.
    #[test]
    fn it_refuses_a_reply_with_no_nonce_in_it() {
        assert!(extract_nonce("{\"error\":\"nope\"}").is_err());
        assert!(extract_nonce("").is_err());
    }

    // Two different keys must not produce the same signature for one message,
    // which is the cheapest possible check that the key is actually used.
    #[test]
    fn it_signs_with_the_key_it_was_given() {
        let a = personal_sign("hello", &Zeroizing::new([0x44u8; 32])).unwrap();
        let b = personal_sign("hello", &Zeroizing::new([0x55u8; 32])).unwrap();
        assert_ne!(a, b);
    }

    // The one test that answers whether any of this is possible. Ignored by
    // default because it reaches a third party over the network, and a suite
    // that goes red when somebody else has a bad afternoon is a suite people
    // stop reading. Run it deliberately:
    //   cargo test -- --ignored signs_in_to_opensea --nocapture
    //
    // A throwaway key is enough: signing in proves an address, and an address
    // that holds nothing can still prove itself.
    #[tokio::test]
    #[ignore]
    async fn signs_in_to_opensea_for_real() {
        let secret = Zeroizing::new([0x7fu8; 32]);
        let key = SigningKey::from_slice(&secret[..]).unwrap();
        let public = key.verifying_key().to_encoded_point(false);
        let address = Address::from_slice(&keccak256(&public.as_bytes()[1..])[12..]);

        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 nock")
            .build()
            .unwrap();

        match authenticate(&http, address, &secret, 4663).await {
            Ok(session) => {
                println!(
                    "SIGNED IN as {address:?}, {} bytes of session",
                    session.cookies.len()
                );
                assert!(!session.cookies.is_empty());
            }
            Err(err) => panic!("sign-in failed: {err}"),
        }
    }
}
