//! NIP-19 entity decoding: npub, note, nprofile, nevent.
//!
//! Handles both simple bech32-encoded entities (npub, note) and TLV-encoded
//! entities (nprofile, nevent) per <https://github.com/nostr-protocol/nips/blob/master/19.md>.

use std::collections::HashMap;

use bech32::FromBase32;
use serde::Serialize;

/// A decoded Nostr entity.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NostrEntity {
    Profile {
        pubkey: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        relays: Vec<String>,
    },
    Event {
        id: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        relays: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<u32>,
    },
}

/// Try to decode a string as a Nostr entity (npub, note, nprofile, nevent)
/// or as a raw 64-char hex identifier.
///
/// Returns `None` if the input is not a recognizable entity.
pub fn decode(input: &str) -> Option<NostrEntity> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Raw 64-char hex — ambiguous (could be pubkey or event id), return as-is
    // The caller decides how to resolve it.
    if is_hex64(trimmed) {
        return None; // Handled separately by the caller with DB lookups
    }

    // Try bech32 decode
    let (hrp, data, _) = bech32::decode(trimmed).ok()?;
    let bytes = Vec::<u8>::from_base32(&data).ok()?;

    match hrp.as_str() {
        "npub" => {
            if bytes.len() != 32 {
                return None;
            }
            Some(NostrEntity::Profile {
                pubkey: hex::encode(&bytes),
                relays: vec![],
            })
        }
        "note" => {
            if bytes.len() != 32 {
                return None;
            }
            Some(NostrEntity::Event {
                id: hex::encode(&bytes),
                relays: vec![],
                author: None,
                kind: None,
            })
        }
        "nprofile" => {
            let tlv = parse_tlv(&bytes)?;
            let pubkey = tlv
                .get(&0)
                .and_then(|vs| vs.first())
                .filter(|b| b.len() == 32)
                .map(|b| hex::encode(b))?;
            let relays = extract_relay_strings(&tlv);
            Some(NostrEntity::Profile { pubkey, relays })
        }
        "nevent" => {
            let tlv = parse_tlv(&bytes)?;
            let id = tlv
                .get(&0)
                .and_then(|vs| vs.first())
                .filter(|b| b.len() == 32)
                .map(|b| hex::encode(b))?;
            let relays = extract_relay_strings(&tlv);
            let author = tlv
                .get(&2)
                .and_then(|vs| vs.first())
                .filter(|b| b.len() == 32)
                .map(|b| hex::encode(b));
            let kind = tlv
                .get(&3)
                .and_then(|vs| vs.first())
                .filter(|b| b.len() == 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            Some(NostrEntity::Event {
                id,
                relays,
                author,
                kind,
            })
        }
        _ => None,
    }
}

/// Check if a string is a 64-character hex string (pubkey or event id).
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Check if a string looks like a nostr entity prefix.
pub fn looks_like_entity(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("npub1")
        || lower.starts_with("note1")
        || lower.starts_with("nprofile1")
        || lower.starts_with("nevent1")
        || is_hex64(s)
}

/// Parse TLV (Type-Length-Value) encoded data.
fn parse_tlv(data: &[u8]) -> Option<HashMap<u8, Vec<Vec<u8>>>> {
    let mut result: HashMap<u8, Vec<Vec<u8>>> = HashMap::new();
    let mut pos = 0;

    while pos < data.len() {
        if pos + 2 > data.len() {
            return None;
        }
        let typ = data[pos];
        let len = data[pos + 1] as usize;
        pos += 2;
        if pos + len > data.len() {
            return None;
        }
        let value = data[pos..pos + len].to_vec();
        result.entry(typ).or_default().push(value);
        pos += len;
    }

    Some(result)
}

/// Extract relay URL strings from TLV type 1 entries.
fn extract_relay_strings(tlv: &HashMap<u8, Vec<Vec<u8>>>) -> Vec<String> {
    tlv.get(&1)
        .map(|vs| {
            vs.iter()
                .filter_map(|b| String::from_utf8(b.clone()).ok())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_npub() {
        // npub for a known pubkey
        let result = decode("npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6");
        assert!(result.is_some());
        if let Some(NostrEntity::Profile { pubkey, relays }) = result {
            assert_eq!(pubkey.len(), 64);
            assert!(relays.is_empty());
        } else {
            panic!("expected Profile");
        }
    }

    #[test]
    fn test_hex64_not_decoded() {
        let hex = "a".repeat(64);
        assert!(decode(&hex).is_none());
        assert!(is_hex64(&hex));
    }

    #[test]
    fn test_invalid_input() {
        assert!(decode("hello world").is_none());
        assert!(decode("").is_none());
        assert!(decode("npub1invalid").is_none());
    }
}
