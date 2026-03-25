use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine;
use secp256k1::{Message, XOnlyPublicKey, SECP256K1};
use sha2::{Digest, Sha256};
use std::str::FromStr;

use crate::api::AppState;
use crate::error::AppError;

/// NIP-98 kind-27235 event for HTTP authentication.
#[derive(Debug, serde::Deserialize)]
struct Nip98Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// Axum extractor that verifies NIP-98 admin authentication.
///
/// Handlers that include `AdminAuth` in their parameters will require
/// a valid `Authorization: Nostr <base64>` header from the admin pubkey.
pub struct AdminAuth {
    pub pubkey: String,
}

impl FromRequestParts<AppState> for AdminAuth {
    type Rejection = AppError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let admin_pubkey = state.admin_pubkey.clone();
        let auth_header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Reconstruct the request URL from parts
        let scheme = "https";
        let host = parts
            .headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let uri = parts.uri.clone();
        let request_url = format!("{scheme}://{host}{uri}");
        let request_method = parts.method.as_str().to_uppercase();

        async move {
            let admin_pubkey = admin_pubkey
                .ok_or_else(|| AppError::Forbidden("admin not configured".into()))?;

            let auth_str = auth_header
                .ok_or_else(|| AppError::Unauthorized("missing Authorization header".into()))?;

            let token = auth_str
                .strip_prefix("Nostr ")
                .ok_or_else(|| AppError::Unauthorized("invalid Authorization scheme".into()))?;

            let json_bytes = base64::engine::general_purpose::STANDARD
                .decode(token)
                .map_err(|_| AppError::Unauthorized("invalid base64 in auth header".into()))?;

            let event: Nip98Event = serde_json::from_slice(&json_bytes)
                .map_err(|_| AppError::Unauthorized("invalid JSON in auth event".into()))?;

            // Verify kind
            if event.kind != 27235 {
                return Err(AppError::Unauthorized("wrong event kind".into()));
            }

            // Verify timestamp (within 60 seconds)
            let now = chrono::Utc::now().timestamp();
            if (now - event.created_at).abs() > 60 {
                return Err(AppError::Unauthorized("auth event expired".into()));
            }

            // Verify URL and method tags
            let mut found_url = false;
            let mut found_method = false;
            for tag in &event.tags {
                if tag.len() >= 2 {
                    match tag[0].as_str() {
                        "u" => {
                            if tag[1] == request_url {
                                found_url = true;
                            }
                        }
                        "method" => {
                            if tag[1].to_uppercase() == request_method {
                                found_method = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !found_url {
                return Err(AppError::Unauthorized("URL tag mismatch".into()));
            }
            if !found_method {
                return Err(AppError::Unauthorized("method tag mismatch".into()));
            }

            // Verify signature
            verify_event_signature(&event)?;

            // Check admin pubkey
            if event.pubkey != admin_pubkey {
                return Err(AppError::Forbidden("not an admin".into()));
            }

            Ok(AdminAuth {
                pubkey: event.pubkey,
            })
        }
    }
}

/// Verify the Schnorr signature of a Nostr event.
fn verify_event_signature(event: &Nip98Event) -> Result<(), AppError> {
    // Canonical serialization: [0, pubkey, created_at, kind, tags, content]
    let serialized = serde_json::json!([
        0,
        &event.pubkey,
        event.created_at,
        event.kind,
        &event.tags,
        &event.content,
    ]);

    let hash = Sha256::digest(serialized.to_string().as_bytes());
    let msg = Message::from_digest_slice(&hash)
        .map_err(|e| AppError::Unauthorized(format!("invalid message hash: {e}")))?;

    let sig = secp256k1::schnorr::Signature::from_str(&event.sig)
        .map_err(|e| AppError::Unauthorized(format!("invalid signature format: {e}")))?;

    let pk = XOnlyPublicKey::from_str(&event.pubkey)
        .map_err(|e| AppError::Unauthorized(format!("invalid pubkey: {e}")))?;

    SECP256K1
        .verify_schnorr(&sig, &msg, &pk)
        .map_err(|_| AppError::Unauthorized("signature verification failed".into()))?;

    Ok(())
}
