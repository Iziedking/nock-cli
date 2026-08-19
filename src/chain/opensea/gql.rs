use alloy_primitives::Address;
use serde::Deserialize;
use thiserror::Error;

use super::siwe::Session;
use super::verify::SubmissionData;

/// The four operations this tool sends, and nothing else.
///
/// Sent as query documents rather than persisted-query hashes. Measured
/// 2026-08-19: `gql.opensea.io` accepts a plain POST, so we carry our own text
/// and their next frontend deploy cannot rotate a hash out from under us.
/// Introspection is disabled on their side, so these documents are the schema
/// as far as this repository is concerned and any drift shows up as a failing
/// fixture test rather than as a surprise at T-0.
const ENDPOINT: &str = "https://gql.opensea.io/graphql?app_id=os2-web";

pub const COLLECTION_SEARCH: &str = r"
query MintCollectionSearch($query: String!) {
  collectionsByQuery(query: $query, limit: 50) {
    __typename
    slug
    address
    chain { identifier networkId }
  }
}
";

pub const COLLECTION_METADATA: &str = r"
query MintCollectionMetadata($slug: String!) {
  collectionBySlug(slug: $slug) {
    __typename
    ... on Collection {
      slug
      address
      chain { identifier networkId }
      drop {
        __typename
        identifier { contractAddress chain { identifier } }
        stages {
          __typename
          stageType
          stageIndex
          startTime
          endTime
          maxTotalMintableByWallet
        }
      }
    }
  }
}
";

pub const DROP_ELIGIBILITY: &str = r"
query DropEligibilityQuery($collectionSlug: String!, $address: Address!) {
  dropBySlug(slug: $collectionSlug) {
    __typename
    ... on Erc721SeaDropV1 { minterQuantityMinted(minter: $address) }
    stages {
      __typename
      stageType
      stageIndex
      isEligible
      eligibleMinterAddress
      maxTotalMintableByWallet
      eligibleMaxTotalMintableByWallet
      eligiblePrice { usd token { unit symbol contractAddress chain { identifier } } }
    }
  }
}
";

pub const MINT_ACTION: &str = r"
query MintActionTimelineQuery(
  $address: Address!
  $fromAssets: [AssetQuantityInput!]!
  $toAssets: [AssetQuantityInput!]!
  $recipient: Address
) {
  swap(
    address: $address
    fromAssets: $fromAssets
    toAssets: $toAssets
    recipient: $recipient
    action: MINT
  ) {
    actions {
      __typename
      ... on TransactionAction {
        transactionSubmissionData {
          to
          data
          value
          chain { networkId identifier }
        }
      }
    }
    errors { __typename }
  }
}
";

/// The native token, which is what a mint is paid from.
pub const NATIVE_TOKEN: &str = "0x0000000000000000000000000000000000000000";

/// Variables for `MintActionTimelineQuery`.
///
/// Shape measured 2026-08-19 by asking the server what it required, one field at
/// a time: `AssetQuantityInput` needs `asset: AssetIdentifier!`, which needs
/// `chain` and `contractAddress`. The two sides must differ, so a mint is the
/// native token in and the collection out.
///
/// THE QUANTITY IS NOT OPTIONAL. Leaving it out was tried against the live
/// service and returns perfectly valid calldata with a quantity word of zero and
/// a value of zero, which is a transaction that succeeds and mints nothing. The
/// price only appears once a quantity is asked for.
pub fn mint_action_variables(
    minter: Address,
    collection: Address,
    chain: &str,
    quantity: u64,
) -> serde_json::Value {
    serde_json::json!({
        "address": format!("{minter:?}"),
        "recipient": format!("{minter:?}"),
        "fromAssets": [{ "asset": { "chain": chain, "contractAddress": NATIVE_TOKEN } }],
        "toAssets": [{
            "asset": { "chain": chain, "contractAddress": format!("{collection:?}") },
            "quantity": quantity.to_string(),
        }],
    })
}

#[derive(Debug, Error)]
pub enum GqlError {
    #[error("could not reach OpenSea: {0}")]
    Transport(String),
    #[error("OpenSea answered {status}")]
    Status { status: u16 },
    #[error("OpenSea returned an error: {0}")]
    Query(String),
    #[error("the reply from OpenSea is missing {0}, which means the shape changed")]
    Missing(&'static str),
    #[error("OpenSea returned an unknown stage type: {0}")]
    UnknownStageType(String),
    #[error("could not read OpenSea's reply: {0}")]
    Malformed(String),
    #[error("{0} matches more than one collection, so which one to mint is not ours to guess")]
    Ambiguous(String),
    #[error("no collection on OpenSea has the address {0}")]
    NotFound(String),
    #[error("a wallet session is needed for this, and none was established")]
    SessionRequired,
    // Not a schema problem: OpenSea understood the question and answered that
    // this mint cannot happen. Kept as their own type name because that is more
    // precise than any sentence we would write over it.
    #[error("OpenSea will not build this mint: {0}")]
    Refused(String),
}

/// The three stage kinds `OpenSea` names, and nothing invented.
///
/// A fourth appearing is news, not something to shrug at: treating an unknown
/// kind as public would mint into a stage nobody checked the rules of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageType {
    PublicSale,
    SignedPresale,
    MerklePresale,
}

impl StageType {
    fn parse(raw: &str) -> Result<Self, GqlError> {
        match raw {
            "PUBLIC_SALE" => Ok(Self::PublicSale),
            "SIGNED_PRESALE" => Ok(Self::SignedPresale),
            "MERKLE_PRESALE" => Ok(Self::MerklePresale),
            other => Err(GqlError::UnknownStageType(other.to_owned())),
        }
    }

    pub const fn is_signed(self) -> bool {
        matches!(self, Self::SignedPresale)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRef {
    pub slug: String,
    pub address: Address,
    pub network_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageMeta {
    pub stage_index: u64,
    pub stage_type: StageType,
    /// Unix seconds. `OpenSea` sends ISO 8601 strings; they are converted here so
    /// nothing downstream has to know that.
    pub start_time: u64,
    pub end_time: u64,
    pub max_total_mintable_by_wallet: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligibility {
    pub stage_index: u64,
    pub stage_type: StageType,
    /// `null` from `OpenSea` when the wallet is not on the list, which is not the
    /// same as `false` and must never default to true.
    pub is_eligible: bool,
    pub eligible_minter: Option<Address>,
    pub max_total_mintable_by_wallet: Option<u64>,
    /// Advisory only, and deliberately kept as `OpenSea` sent it.
    ///
    /// The price this run actually commits to is the one inside the calldata,
    /// checked against the floor the collection published on chain and against
    /// `--max-spend`. Converting a display string into wei here and then trusting
    /// it would put a rounding decision on the money path for no gain.
    pub quoted_price: Option<String>,
}

// The wire shapes. Only the fields this tool reads are declared, so a field
// added on their side is ignored rather than fatal, and a field removed that we
// depend on is a deserialise error rather than a silent default.
#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    message: String,
}

#[derive(Deserialize)]
struct SearchData {
    #[serde(rename = "collectionsByQuery")]
    collections_by_query: Vec<WireCollection>,
}

#[derive(Deserialize)]
struct WireCollection {
    slug: String,
    address: String,
    chain: WireChain,
}

#[derive(Deserialize)]
struct WireChain {
    #[serde(rename = "networkId")]
    network_id: u64,
}

#[derive(Deserialize)]
struct MetadataData {
    #[serde(rename = "collectionBySlug")]
    collection_by_slug: Option<WireCollectionWithDrop>,
}

#[derive(Deserialize)]
struct WireCollectionWithDrop {
    drop: Option<WireDrop>,
}

#[derive(Deserialize)]
struct WireDrop {
    stages: Vec<WireStage>,
}

#[derive(Deserialize)]
struct WireStage {
    #[serde(rename = "stageType")]
    stage_type: String,
    #[serde(rename = "stageIndex")]
    stage_index: u64,
    #[serde(rename = "startTime")]
    start_time: String,
    #[serde(rename = "endTime")]
    end_time: String,
    #[serde(rename = "maxTotalMintableByWallet")]
    max_total_mintable_by_wallet: Option<u64>,
}

#[derive(Deserialize)]
struct EligibilityData {
    #[serde(rename = "dropBySlug")]
    drop_by_slug: Option<WireEligibleDrop>,
}

#[derive(Deserialize)]
struct WireEligibleDrop {
    stages: Vec<WireEligibleStage>,
}

#[derive(Deserialize)]
struct WireEligibleStage {
    #[serde(rename = "stageType")]
    stage_type: String,
    #[serde(rename = "stageIndex")]
    stage_index: u64,
    #[serde(rename = "isEligible")]
    is_eligible: Option<bool>,
    #[serde(rename = "eligibleMinterAddress")]
    eligible_minter_address: Option<String>,
    #[serde(rename = "eligibleMaxTotalMintableByWallet")]
    eligible_max: Option<u64>,
    #[serde(rename = "eligiblePrice")]
    eligible_price: Option<WirePrice>,
}

#[derive(Deserialize)]
struct WirePrice {
    token: Option<WireToken>,
}

#[derive(Deserialize)]
struct WireToken {
    unit: Option<String>,
}

#[derive(Deserialize)]
struct SwapData {
    swap: Option<WireSwap>,
}

#[derive(Deserialize)]
struct WireSwap {
    actions: Vec<WireAction>,
    #[serde(default)]
    errors: Vec<WireSwapError>,
}

#[derive(Deserialize)]
struct WireSwapError {
    #[serde(rename = "__typename")]
    typename: String,
}

#[derive(Deserialize)]
struct WireAction {
    #[serde(rename = "transactionSubmissionData")]
    submission: Option<WireSubmission>,
}

#[derive(Deserialize)]
struct WireSubmission {
    to: String,
    data: String,
    value: Option<String>,
}

fn envelope<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, GqlError> {
    let parsed: Envelope<T> =
        serde_json::from_str(json).map_err(|e| GqlError::Malformed(e.to_string()))?;
    if !parsed.errors.is_empty() {
        let joined = parsed
            .errors
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        // UNAUTHORIZED on the eligibility fields is the ordinary "not signed in"
        // case and deserves its own error rather than a wall of GraphQL noise.
        if joined.contains("Access denied") {
            return Err(GqlError::SessionRequired);
        }
        return Err(GqlError::Query(joined));
    }
    parsed.data.ok_or(GqlError::Missing("data"))
}

/// ISO 8601 to unix seconds, for the four timestamp fields `OpenSea` sends.
///
/// Only the shape they actually send is accepted, `YYYY-MM-DDTHH:MM:SS` with an
/// optional fractional part and a trailing Z. Anything else is an error rather
/// than a guess, because a mistimed stage is the one failure this whole tool
/// exists to avoid.
fn iso_to_unix(text: &str) -> Result<u64, GqlError> {
    let bad = || GqlError::Malformed(format!("{text} is not a timestamp this understands"));
    let (date, rest) = text.split_once('T').ok_or_else(bad)?;
    let time = rest.split(['.', 'Z', '+']).next().ok_or_else(bad)?;

    let mut d = date.split('-');
    let year: i64 = d.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i64 = d.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i64 = d.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;

    let mut t = time.split(':');
    let hour: i64 = t.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let minute: i64 = t.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let second: i64 = t.next().unwrap_or("0").parse().map_err(|_| bad())?;

    // Howard Hinnant's days_from_civil, the same arithmetic as the formatter in
    // siwe.rs read backwards.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).map_err(|_| bad())
}

/// The collection whose address matches, from a text search that returns many.
///
/// Refuses an ambiguous result rather than taking the first: two collections
/// sharing an address on one chain is not something to resolve by ordering.
pub fn parse_collection(json: &str, wanted: Address) -> Result<CollectionRef, GqlError> {
    let data: SearchData = envelope(json)?;
    let mut matches = data
        .collections_by_query
        .into_iter()
        .filter(|c| c.address.parse::<Address>().is_ok_and(|a| a == wanted))
        .collect::<Vec<_>>();

    match matches.len() {
        0 => Err(GqlError::NotFound(format!("{wanted:?}"))),
        1 => {
            let found = matches.remove(0);
            Ok(CollectionRef {
                slug: found.slug,
                address: wanted,
                network_id: found.chain.network_id,
            })
        }
        _ => Err(GqlError::Ambiguous(format!("{wanted:?}"))),
    }
}

pub fn parse_metadata(json: &str) -> Result<Vec<StageMeta>, GqlError> {
    let data: MetadataData = envelope(json)?;
    let drop = data
        .collection_by_slug
        .and_then(|c| c.drop)
        .ok_or(GqlError::Missing("drop"))?;

    drop.stages
        .into_iter()
        .map(|s| {
            Ok(StageMeta {
                stage_index: s.stage_index,
                stage_type: StageType::parse(&s.stage_type)?,
                start_time: iso_to_unix(&s.start_time)?,
                end_time: iso_to_unix(&s.end_time)?,
                max_total_mintable_by_wallet: s.max_total_mintable_by_wallet.unwrap_or(0),
            })
        })
        .collect()
}

pub fn parse_eligibility(json: &str) -> Result<Vec<Eligibility>, GqlError> {
    let data: EligibilityData = envelope(json)?;
    let drop = data.drop_by_slug.ok_or(GqlError::Missing("dropBySlug"))?;

    drop.stages
        .into_iter()
        .map(|s| {
            Ok(Eligibility {
                stage_index: s.stage_index,
                stage_type: StageType::parse(&s.stage_type)?,
                // null means not on the list. Defaulting the other way would
                // enter wallets into stages they cannot mint.
                is_eligible: s.is_eligible.unwrap_or(false),
                eligible_minter: s
                    .eligible_minter_address
                    .and_then(|a| a.parse::<Address>().ok()),
                max_total_mintable_by_wallet: s.eligible_max,
                quoted_price: s.eligible_price.and_then(|p| p.token).and_then(|t| t.unit),
            })
        })
        .collect()
}

/// The calldata, and only from a `TransactionAction` that carries one.
pub fn parse_submission(json: &str) -> Result<SubmissionData, GqlError> {
    let data: SwapData = envelope(json)?;
    let swap = data.swap.ok_or(GqlError::Missing("swap"))?;
    // Errors here are answers, not failures of ours: InsufficientFundError,
    // ineligibility, a closed stage. Reported before the missing-calldata check
    // so the reason is the real one rather than "no calldata".
    if !swap.errors.is_empty() {
        return Err(GqlError::Refused(
            swap.errors
                .iter()
                .map(|e| e.typename.clone())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    let submission = swap
        .actions
        .into_iter()
        .find_map(|a| a.submission)
        .ok_or(GqlError::Missing("transactionSubmissionData"))?;

    let to: Address = submission
        .to
        .parse()
        .map_err(|_| GqlError::Malformed(format!("{} is not an address", submission.to)))?;
    let data_bytes = hex::decode(submission.data.trim_start_matches("0x"))
        .map_err(|e| GqlError::Malformed(format!("calldata is not hex: {e}")))?;
    let value_wei = match submission.value {
        Some(v) if !v.is_empty() => v
            .parse::<u128>()
            .map_err(|_| GqlError::Malformed(format!("{v} is not an amount of wei")))?,
        _ => 0,
    };

    Ok(SubmissionData {
        to,
        data: data_bytes,
        value_wei,
    })
}

/// Posts one operation and returns the raw body.
pub async fn post(
    http: &reqwest::Client,
    query: &str,
    variables: serde_json::Value,
    session: Option<&Session>,
) -> Result<String, GqlError> {
    let mut request = http
        .post(ENDPOINT)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("origin", "https://opensea.io")
        .header("referer", "https://opensea.io/");
    if let Some(session) = session {
        request = request.header("cookie", session.cookies.clone());
    }

    let reply = request
        .json(&serde_json::json!({ "query": query, "variables": variables }))
        .send()
        .await
        .map_err(|e| GqlError::Transport(e.to_string()))?;

    let status = reply.status();
    let body = reply
        .text()
        .await
        .map_err(|e| GqlError::Transport(e.to_string()))?;
    if !status.is_success() {
        return Err(GqlError::Status {
            status: status.as_u16(),
        });
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from the live service on 2026-08-19, not written by hand. A shape
    // change on their side turns these red, which is the whole reason this
    // directory exists.
    const SEARCH: &str =
        include_str!("../../../tests/fixtures/opensea/mint_collection_search.json");
    const METADATA: &str =
        include_str!("../../../tests/fixtures/opensea/mint_collection_metadata.json");
    const ELIGIBILITY: &str = include_str!("../../../tests/fixtures/opensea/drop_eligibility.json");

    const SUSHI: &str = "0x941c2a17c60ad6daf86cb6438074d57e906adffa";

    #[test]
    fn it_finds_the_collection_whose_address_matches() {
        let found = parse_collection(SEARCH, SUSHI.parse().unwrap()).unwrap();
        assert_eq!(found.slug, "sushicatart");
        assert_eq!(found.network_id, 4663);
    }

    #[test]
    fn it_refuses_an_address_no_collection_has() {
        let other: Address = "0x000000000000000000000000000000000000dead"
            .parse()
            .unwrap();
        assert!(matches!(
            parse_collection(SEARCH, other),
            Err(GqlError::NotFound(_))
        ));
    }

    // Two collections on one address is not something to resolve by taking the
    // first result.
    #[test]
    fn it_refuses_an_ambiguous_match_rather_than_guessing() {
        let doubled = SEARCH.replace(
            "\"collectionsByQuery\": [",
            &format!(
                "\"collectionsByQuery\": [{{\"__typename\":\"Collection\",\"slug\":\"impostor\",\"address\":\"{SUSHI}\",\"chain\":{{\"identifier\":\"robinhood\",\"networkId\":4663}}}},"
            ),
        );
        assert!(matches!(
            parse_collection(&doubled, SUSHI.parse().unwrap()),
            Err(GqlError::Ambiguous(_))
        ));
    }

    #[test]
    fn it_reads_every_stage_with_its_type_and_window() {
        let stages = parse_metadata(METADATA).unwrap();
        assert!(!stages.is_empty());
        assert!(stages
            .iter()
            .any(|s| s.stage_type == StageType::SignedPresale));
        for s in &stages {
            assert!(
                s.end_time >= s.start_time,
                "stage {} ends before it starts",
                s.stage_index
            );
        }
    }

    // OpenSea sends ISO 8601 and the rest of the tool works in unix seconds. The
    // value below is the one the chain independently published as minStartTime
    // for the same stage, so the two sources agree.
    #[test]
    fn it_converts_iso_timestamps_to_unix_seconds() {
        assert_eq!(
            iso_to_unix("2026-08-17T11:39:49.000Z").unwrap(),
            1_786_966_789
        );
        assert_eq!(iso_to_unix("1970-01-01T00:00:00.000Z").unwrap(), 0);
        assert_eq!(
            iso_to_unix("2024-02-29T00:00:00.000Z").unwrap(),
            1_709_164_800
        );
    }

    #[test]
    fn it_refuses_a_timestamp_it_does_not_understand() {
        assert!(iso_to_unix("yesterday").is_err());
        assert!(iso_to_unix("").is_err());
    }

    // A fourth stage type appearing is news. Treating it as public would mint
    // into a stage whose rules nothing checked.
    #[test]
    fn it_refuses_a_stage_type_it_does_not_know() {
        let changed = METADATA.replace("SIGNED_PRESALE", "SOMETHING_NEW");
        assert!(matches!(
            parse_metadata(&changed),
            Err(GqlError::UnknownStageType(_))
        ));
    }

    #[test]
    fn it_reads_eligibility_per_stage() {
        let all = parse_eligibility(ELIGIBILITY).unwrap();
        assert!(!all.is_empty());
        assert!(all.iter().any(|e| e.stage_type == StageType::SignedPresale));
    }

    // The subtlety that would otherwise cost a mint: OpenSea answers null, not
    // false, for a wallet that is not on the list.
    #[test]
    fn it_treats_a_null_eligibility_as_not_eligible() {
        let all = parse_eligibility(ELIGIBILITY).unwrap();
        assert!(
            all.iter().all(|e| !e.is_eligible),
            "the fixture wallet is on no list, so nothing may read as eligible"
        );
    }

    #[test]
    fn it_reads_an_explicit_true_as_eligible() {
        let changed = ELIGIBILITY.replacen("\"isEligible\": null", "\"isEligible\": true", 1);
        let all = parse_eligibility(&changed).unwrap();
        assert_eq!(all.iter().filter(|e| e.is_eligible).count(), 1);
    }

    // Not signing in is an ordinary state with its own message, not a wall of
    // GraphQL noise.
    #[test]
    fn it_names_a_missing_session_rather_than_repeating_graphql() {
        let denied =
            r#"{"errors":[{"message":"Access denied","extensions":{"code":"UNAUTHORIZED"}}]}"#;
        assert!(matches!(
            parse_eligibility(denied),
            Err(GqlError::SessionRequired)
        ));
    }

    #[test]
    fn it_reads_the_submission_data_as_bytes_not_a_string() {
        let json = r#"{"data":{"swap":{"actions":[{"__typename":"TransactionAction",
            "transactionSubmissionData":{"to":"0x00005EA00Ac477B1030CE78506496e8C2dE24bf5",
            "data":"0x4b61cd6f00","value":"15000000000000"}}],"errors":[]}}}"#;
        let s = parse_submission(json).unwrap();
        assert_eq!(&s.data[0..4], &[0x4b, 0x61, 0xcd, 0x6f]);
        assert_eq!(s.value_wei, 15_000_000_000_000);
    }

    #[test]
    fn it_reads_a_missing_value_as_free_rather_than_failing() {
        let json = r#"{"data":{"swap":{"actions":[{"transactionSubmissionData":
            {"to":"0x00005EA00Ac477B1030CE78506496e8C2dE24bf5","data":"0x4b61cd6f","value":null}}]}}}"#;
        assert_eq!(parse_submission(json).unwrap().value_wei, 0);
    }

    // A shape change is what this whole directory exists to catch, so it has to
    // be an error and never a default.
    #[test]
    fn it_fails_loudly_when_the_calldata_field_has_gone() {
        let json = r#"{"data":{"swap":{"actions":[{"somethingElse":{}}]}}}"#;
        assert!(matches!(
            parse_submission(json),
            Err(GqlError::Missing("transactionSubmissionData"))
        ));
    }

    // OpenSea answering "this mint cannot happen" is an answer, and saying
    // "missing transactionSubmissionData" over the top of it would report our
    // parser's disappointment instead of their reason.
    #[test]
    fn it_reports_why_opensea_would_not_build_the_mint() {
        let json =
            r#"{"data":{"swap":{"actions":[],"errors":[{"__typename":"InsufficientFundError"}]}}}"#;
        match parse_submission(json) {
            Err(GqlError::Refused(why)) => assert_eq!(why, "InsufficientFundError"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn it_builds_the_mint_variables_in_the_shape_the_server_requires() {
        let v = mint_action_variables(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
            SUSHI.parse().unwrap(),
            "robinhood",
            3,
        );
        assert_eq!(v["fromAssets"][0]["asset"]["contractAddress"], NATIVE_TOKEN);
        assert_eq!(v["toAssets"][0]["asset"]["contractAddress"], SUSHI);
        assert_eq!(v["fromAssets"][0]["asset"]["chain"], "robinhood");
        assert_eq!(v["address"], v["recipient"]);
    }

    // Measured against the live service: omitting the quantity returns valid
    // calldata whose quantity word is zero and whose value is zero. That is a
    // transaction which succeeds and mints nothing, which is the worst kind of
    // wrong, so a quantity is always sent.
    #[test]
    fn it_always_asks_for_a_quantity_because_omitting_it_mints_nothing() {
        let v = mint_action_variables(
            "0x00000000000000000000000000000000000000aa"
                .parse()
                .unwrap(),
            SUSHI.parse().unwrap(),
            "robinhood",
            2,
        );
        assert_eq!(v["toAssets"][0]["quantity"], "2");
    }

    #[test]
    fn it_surfaces_a_graphql_error_rather_than_an_empty_result() {
        let json = r#"{"errors":[{"message":"Cannot query field \"nope\""}]}"#;
        assert!(matches!(parse_metadata(json), Err(GqlError::Query(_))));
    }

    // The whole client against the real service: sign in, resolve an address to
    // a slug, read its stages, then ask a wallet-specific question that only an
    // authenticated session can answer.
    //
    // Ignored by default, like the other network tests. Run it when something
    // OpenSea-shaped looks wrong:
    //   cargo test -- --ignored walks_the_whole_opensea_path --nocapture
    #[tokio::test]
    #[ignore]
    async fn walks_the_whole_opensea_path_for_real() {
        use crate::chain::opensea::siwe::authenticate;
        use k256::ecdsa::SigningKey;
        use zeroize::Zeroizing;

        let secret = Zeroizing::new([0x7fu8; 32]);
        let key = SigningKey::from_slice(&secret[..]).unwrap();
        let public = key.verifying_key().to_encoded_point(false);
        let address =
            Address::from_slice(&alloy_primitives::keccak256(&public.as_bytes()[1..])[12..]);

        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 nock")
            .build()
            .unwrap();

        let wanted: Address = SUSHI.parse().unwrap();
        let body = post(
            &http,
            COLLECTION_SEARCH,
            serde_json::json!({ "query": SUSHI }),
            None,
        )
        .await
        .unwrap();
        let found = parse_collection(&body, wanted).unwrap();
        println!("resolved {SUSHI} to {}", found.slug);
        assert_eq!(found.network_id, 4663);

        let body = post(
            &http,
            COLLECTION_METADATA,
            serde_json::json!({ "slug": found.slug }),
            None,
        )
        .await
        .unwrap();
        let stages = parse_metadata(&body).unwrap();
        println!("{} stage(s)", stages.len());
        assert!(!stages.is_empty());

        // Without a session this must name the missing session rather than
        // returning an empty answer that reads like ineligibility.
        let body = post(
            &http,
            DROP_ELIGIBILITY,
            serde_json::json!({ "collectionSlug": "mr-machine", "address": format!("{address:?}") }),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            parse_eligibility(&body),
            Err(GqlError::SessionRequired)
        ));

        let session = authenticate(&http, address, &secret, 4663).await.unwrap();
        let body = post(
            &http,
            DROP_ELIGIBILITY,
            serde_json::json!({ "collectionSlug": "mr-machine", "address": format!("{address:?}") }),
            Some(&session),
        )
        .await
        .unwrap();
        let eligibility = parse_eligibility(&body).unwrap();
        println!("{} stage(s) answered with a session", eligibility.len());
        assert!(!eligibility.is_empty());
    }

    // Proves the mint-action shape against the real service. An empty wallet
    // cannot receive calldata, so the pass condition is a refusal with their own
    // reason rather than a schema complaint: a wrong shape fails validation,
    // and a right shape fails on funds.
    #[tokio::test]
    #[ignore]
    async fn the_mint_action_query_is_understood_by_the_real_service() {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 nock")
            .build()
            .unwrap();
        let minter: Address = "0xa1d79dfa76e98d5e8a776114d9524c4b6e888daa"
            .parse()
            .unwrap();
        let body = post(
            &http,
            MINT_ACTION,
            mint_action_variables(minter, SUSHI.parse().unwrap(), "robinhood", 1),
            None,
        )
        .await
        .unwrap();

        match parse_submission(&body) {
            Err(GqlError::Refused(why)) => println!("understood, and refused with: {why}"),
            Ok(s) => println!(
                "understood, and returned {} bytes of calldata",
                s.data.len()
            ),
            Err(other) => panic!("the query was not understood: {other}"),
        }
    }
}
