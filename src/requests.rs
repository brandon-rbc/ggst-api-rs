use crate::{error::*, *};
use chrono::{DateTime, NaiveDateTime, Utc};
use hex::ToHex;
use lazy_static::lazy_static;
use regex::{bytes, Regex};
use reqwest::{self, header};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::str;

const DEFAULT_UTILS_BASE_URL: &str =
    "https://ggst-utils-default-rtdb.europe-west1.firebasedatabase.app";
const DEFAULT_BASE_URL: &str = "https://ggst-game.guiltygear.com";

/// Context struct which contains the base urls used for api requests. Use the associated methods
/// to overwrite urls if necessary.
pub struct Context {
    base_url: String,
    utils_base_url: String,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            base_url: DEFAULT_BASE_URL.to_string(),
            utils_base_url: DEFAULT_UTILS_BASE_URL.to_string(),
        }
    }
}

impl Context {
    pub fn new() -> Self {
        Context::default()
    }

    /// Overwrite the url used for api requests. The default is https://ggst-game.guiltygear.com
    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Overwrite the url used for requests regarding static content, such as user ids. The default
    /// is https://ggst-utils-default-rtdb.europe-west1.firebasedatabase.app
    pub fn utils_base_url(mut self, utils_base_url: String) -> Self {
        self.utils_base_url = utils_base_url;
        self
    }
}

/// Retrieve the latest set of replays. Each page contains approximately 10 replays, however this is not
/// guaranteed. Indicate the min and maximum floor you want to query.
/// No more than 100 pages can be queried at a time. If no matches can be found the parsing will
/// fail. Usually a few replays have weird timestamps from the future. It is recommended to apply a
/// filter on the current time before using any matches, like `.filter(|m| m.timestamp() <
/// &chrono::Utc::now())`
pub async fn get_replays(
    context: &Context,
    pages: usize,
    min_floor: Floor,
    max_floor: Floor,
) -> Result<impl Iterator<Item = Match>> {
    // Check for invalid inputs
    if pages > 100 {
        return Err(Error::InvalidArguments(format!(
            "pages: {} Cannot query more than 100 pages",
            pages
        )));
    }
    if min_floor > max_floor {
        return Err(Error::InvalidArguments(format!(
            "min_floor {:?} is larger than max_floor {:?}",
            min_floor, max_floor
        )));
    }

    let request_url = format!("{}/api/catalog/get_replay", context.base_url);
    let client = reqwest::Client::new();

    // Assume at most 10 replays per page for pre allocation
    let mut matches = BTreeSet::new();
    for i in 0..pages {
        // Construct the query string
        let hex_index = format!("{:02X}", i);
        let query_string = format!(
            "9295B2323131303237313133313233303038333834AD3631613565643466343631633202A5302E302E38039401CC{}0A9AFF00{}{}90FFFF000001",
            hex_index,
            min_floor.to_hex(),
            max_floor.to_hex());
        let response = client
            .post(&request_url)
            .header(header::USER_AGENT, "Steam")
            .header(header::CACHE_CONTROL, "no-cache")
            .form(&[("data", query_string)])
            .send()
            .await?;

        // Regex's to parse the raw bytes received
        lazy_static! {
            // This separates the matches from each other
            static ref MATCH_SEP: bytes::Regex =
                bytes::Regex::new(r"(?-u)\x01\x00\x00\x00")
                    .expect("Could not compile regex");
            // The separator which separates data within a match segment
            static ref PLAYER_DATA_START: bytes::Regex = bytes::Regex::new(r"(?-u)\x95\xb2").expect("Could not compile regex");
        }

        // Convert the response to raw bytes
        let bytes = response.bytes().await?;

        // Check if only the header is present
        // If yes then we found no matches and return early
        // The function should not fail but rather return an empty set or what was already found
        if bytes.len() < 63 {
            return Ok(matches.into_iter());
        }

        // Remove the first 61 bytes, they are static header, we don't need them
        let bytes = bytes.slice(61..);

        // Split on the match separator and keep non empty results only
        // This should give us 10 separate matches
        for raw_match in MATCH_SEP.split(&bytes).filter(|b| !b.is_empty()) {
            // Structure of the data to be extracted:
            // We have three sections that have to be parsed
            // Section 1: {floor}{p1_char}{p2_char}
            // Section 2: \x95\xb2{p1_id [18 chars]}\xa_{p1_name}\xb1{p1_some_number}\xaf{p1_online_id}\x07
            // Section 3: \x95\xb2{p2_id}\xa_{p2_name}\xb1{p2_some_number}\xaf{p2_online_id}\t{winner}\xb3{timestamp}

            // Split the match data on the player separator
            let mut data = PLAYER_DATA_START
                .split(raw_match)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take(3)
                .rev();

            // Section 1
            let (floor, p1_char, p2_char) = match data.next() {
                Some(b) => {
                    let n = b.len();
                    if n < 3 {
                        return Err(Error::UnexpectedResponse(
                            "First data part does not have 3 bytes".into(),
                        ));
                    }
                    (b[n - 3], b[n - 2], b[n - 1])
                }
                None => {
                    return Err(Error::UnexpectedResponse(
                        "Could not find first data part of response".into(),
                    ))
                }
            };

            // Section 2
            let (p1_id, p1_name) = match data.next() {
                Some(b) => {
                    // We check if the array is long enough
                    // it has to be at least 18 characters for the player user_id
                    // one character for the separator \xa_ and then at least 1 byte for
                    // the username
                    if b.len() < 20 {
                        return Err(Error::UnexpectedResponse(format!(
                            "Second data part does not have 20 bytes, has {} instead: {} in {}",
                            b.len(),
                            show_buf(b),
                            show_buf(raw_match)
                        )));
                    }

                    let name = match b[19..].split(|f| *f == b'\xb1').next() {
                        Some(name_bytes) => String::from_utf8_lossy(name_bytes),
                        None => {
                            return Err(Error::UnexpectedResponse(format!(
                                "Could not parse player1 name: {}",
                                show_buf(&b[19..])
                            )))
                        }
                    };
                    (String::from_utf8_lossy(&b[0..18]), name)
                }
                None => {
                    return Err(Error::UnexpectedResponse(
                        "Could not find second data part of response".into(),
                    ))
                }
            };

            // Section 3
            let (p2_id, p2_name, winner, time) = match data.next() {
                Some(b) => {
                    // We check if the array is long enough, 76 characters required for a 1 byte
                    // username, it has to be at least 76 characters for the player user_id, online_id,
                    // timestamp, the other number and the winner indicator and separators
                    // and then at least 1 byte for the username
                    // There do exist weird edge cases where the third data part does not contain
                    // an online id, instead it has a dummy user name, this will then take 71 bytes
                    // instead
                    if b.len() < 71 {
                        return Err(Error::UnexpectedResponse(format!(
                            "Third data part does not have 71 bytes, has {} instead: {} in {}",
                            b.len(),
                            show_buf(b),
                            show_buf(raw_match)
                        )));
                    }

                    let name = match b[19..].split(|f| *f == b'\xb1').next() {
                        Some(name_bytes) => String::from_utf8_lossy(name_bytes),
                        None => {
                            return Err(Error::UnexpectedResponse(format!(
                                "Could not parse player2 name: {}",
                                show_buf(&b[19..])
                            )))
                        }
                    };

                    // first 38 bytes are unnecessary as they contain the username and id's
                    // \xb3 is in front of the timestamp, so we split the bytes on that and take
                    // the last two segements, which should be the winner and timestamp
                    // This can break if there are more bytes behind the timestamp that contain the
                    // \xb3 byte
                    let winner_time_bytes = b[38..]
                        .split(|f| *f == b'\xb3')
                        .rev()
                        .take(2)
                        .collect::<Vec<_>>();
                    let time = match winner_time_bytes.get(0) {
                        Some(bytes) => {
                            // 16 bytes before the relevant section
                            // We need 1 byte for the winner, 1 byte for the separator and 19 bytes
                            // for the timestamp
                            if bytes.len() < 19 {
                                return Err(Error::UnexpectedResponse(format!(
                                    "Not enough bytes to parse timestamp: {}",
                                    show_buf(&b[38..])
                                )));
                            }
                            String::from_utf8_lossy(&bytes[0..19])
                        }
                        None => {
                            return Err(Error::UnexpectedResponse(format!(
                                "Could not split bytes to parse winner and timestamp: {}",
                                show_buf(&b[38..])
                            )))
                        }
                    };
                    let winner = match winner_time_bytes.get(1) {
                        Some(bytes) => match bytes.last() {
                            None => {
                                return Err(Error::UnexpectedResponse(format!(
                                    "Could not find winner in bytes: {}",
                                    show_buf(&b[38..])
                                )))
                            }
                            Some(b) => b,
                        },
                        None => {
                            return Err(Error::UnexpectedResponse(format!(
                                "Could not split bytes to parse winner: {}",
                                show_buf(&b[38..])
                            )))
                        }
                    };
                    (String::from_utf8_lossy(&b[0..18]), name, winner, time)
                }
                None => {
                    return Err(Error::UnexpectedResponse(
                        "Could not find third data part of match".into(),
                    ))
                }
            };

            // Construct the match
            let match_data = Match {
                floor: Floor::from_u8(floor)?,
                timestamp: match NaiveDateTime::parse_from_str(&time, "%Y-%m-%d %H:%M:%S") {
                    Ok(t) => DateTime::<Utc>::from_utc(t, Utc),
                    Err(_) => {
                        return Err(Error::UnexpectedResponse(format!(
                            "Could not parse datetime {}",
                            &time
                        )))
                    }
                },
                players: (
                    Player {
                        id: u64::from_str_radix(&p1_id, 10).map_err(|_| {
                            Error::UnexpectedResponse(format!(
                                "Could not parse u64 id from {}",
                                p1_id
                            ))
                        })?,
                        name: p1_name.to_string(),
                        character: Character::from_u8(p1_char)?,
                    },
                    Player {
                        id: u64::from_str_radix(&p2_id, 10).map_err(|_| {
                            Error::UnexpectedResponse(format!(
                                "Could not parse u64 id from {}",
                                p2_id
                            ))
                        })?,
                        name: p2_name.to_string(),
                        character: Character::from_u8(p2_char)?,
                    },
                ),
                winner: match winner {
                    1 => Winner::Player1,
                    2 => Winner::Player2,
                    _ => {
                        return Err(Error::UnexpectedResponse(format!(
                            "Could not parse winner {}",
                            winner
                        )))
                    }
                },
            };

            // Insert it into the set
            matches.insert(match_data);
        }
    }
    Ok(matches.into_iter())
}

async fn userid_from_steamid(context: &Context, steamid: &str) -> Result<String> {
    let request_url = format!("{}/{}.json", context.utils_base_url, steamid);
    let response = reqwest::get(request_url).await?;
    let d: Value = serde_json::from_str(&response.text().await?)?;
    match d.get("UserID") {
        Some(s) => Ok(String::from(
            s.as_str()
                .ok_or(Error::UnexpectedResponse("Could not parse user id".into()))?,
        )),
        None => Err(Error::UnexpectedResponse("Could not parse user id".into())),
    }
}

/// Receive user data from a steamid
pub async fn user_from_steamid(context: &Context, steamid: &str) -> Result<User> {
    // Get the user id from the steamid
    let id = userid_from_steamid(context, steamid).await?;

    // Construct the request with token and appropriate AOB
    let request_url = format!("{}/api/statistics/get", context.base_url);
    let client = reqwest::Client::new();
    let query = format!(
        "9295B2323131303237313133313233303038333834AD3631393064363236383739373702A5302E302E380396B2{}070101FFFFFF",
        id.encode_hex::<String>()
    );
    let response = client
        .post(request_url)
        .form(&[("data", query)])
        .send()
        .await?;

    // Remove invalid unicode stuff before the actual json body
    let content = &response.text().await?;
    lazy_static! {
        static ref RE: Regex = Regex::new(r"[^\{]*\{").expect("Could not compile regex");
    }
    let content = RE.replacen(content, 1, "{");
    let v: Value = serde_json::from_str(&content)?;

    // Assemble the user object
    Ok(User {
        id,
        name: String::from(
            v.get("NickName")
                .ok_or(Error::UnexpectedResponse("Could not parse username".into()))?
                .as_str()
                .ok_or(Error::UnexpectedResponse("Could not parse username".into()))?,
        ),
        comment: String::from(
            v.get("PublicComment")
                .ok_or(Error::UnexpectedResponse(
                    "Could not parse profile comment".into(),
                ))?
                .as_str()
                .ok_or(Error::UnexpectedResponse(
                    "Could not parse profile comment".into(),
                ))?,
        ),
        floor: Floor::Celestial,
        stats: MatchStats { total: 0, wins: 0 },
        celestial_stats: MatchStats { total: 0, wins: 0 },
        char_stats: HashMap::new(),
    })
}

// Helper function for debugging
fn show_buf<B: AsRef<[u8]>>(buf: B) -> String {
    use std::ascii::escape_default;
    String::from_utf8(
        buf.as_ref()
            .iter()
            .map(|b| escape_default(*b))
            .flatten()
            .collect(),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn get_userid() {
        let ctx = Context::new();
        let id = userid_from_steamid(&ctx, "76561198045733267")
            .await
            .unwrap();
        assert_eq!(id, "210611132841904307");
    }

    #[tokio::test]
    async fn get_user_stats() {
        let ctx = Context::new();
        let user = user_from_steamid(&ctx, "76561198045733267").await.unwrap();
        assert_eq!(user.name, "enemy fungus");
    }

    #[tokio::test]
    async fn query_replays() {
        let ctx = Context::new();
        let n_replays = 100;
        let replays = get_replays(&ctx, n_replays, Floor::F1, Floor::Celestial)
            .await
            .unwrap()
            .filter(|m| m.timestamp() < &Utc::now())
            .collect::<Vec<_>>();
        println!("Got {} replays", replays.len());
        if replays.len() > 1 {
            println!("Oldest replay: {}", replays.first().unwrap());
            println!("Latest replay: {}", replays.last().unwrap());
        }
    }
}
