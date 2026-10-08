//! Just enough nostr to trust a sign-up: NIP-01 events and their Schnorr
//! signatures, NIP-98 HTTP auth, and NIP-19 npubs.

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use bech32::{Bech32, Hrp};
use secp256k1::{Message, Secp256k1, XOnlyPublicKey, schnorr::Signature};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// NIP-98's HTTP-auth kind.
pub const HTTP_AUTH_KIND: u64 = 27235;
/// How far a NIP-98 event's `created_at` may be from the server's clock.
pub const MAX_CLOCK_SKEW_SECS: u64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

impl Event {
    /// The id NIP-01 defines: sha256 of the compact JSON
    /// `[0, pubkey, created_at, kind, tags, content]`. serde_json escapes
    /// exactly the characters NIP-01 asks for.
    pub fn computed_id(&self) -> [u8; 32] {
        let canonical = serde_json::json!([
            0,
            self.pubkey,
            self.created_at,
            self.kind,
            self.tags,
            self.content
        ]);
        Sha256::digest(canonical.to_string().as_bytes()).into()
    }

    /// The id matches the content, and the signature is the pubkey's.
    pub fn verify(&self) -> Result<()> {
        let id = self.computed_id();
        ensure!(
            hex::encode(id) == self.id.to_ascii_lowercase(),
            "event id does not match its content"
        );
        let pubkey = XOnlyPublicKey::from_slice(&hex32(&self.pubkey).context("pubkey")?)?;
        let sig = Signature::from_slice(&hex::decode(&self.sig).context("sig")?)?;
        Secp256k1::verification_only()
            .verify_schnorr(&sig, &Message::from_digest(id), &pubkey)
            .context("bad signature")?;
        Ok(())
    }

    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|t| t.first().map(String::as_str) == Some(name))
            .and_then(|t| t.get(1))
            .map(String::as_str)
    }
}

/// Check an `Authorization: Nostr <base64 event>` header for a request to
/// `url` with `method` and `body`, at `now`. Returns the signer's pubkey, hex.
pub fn verify_http_auth(
    header: &str,
    url: &str,
    method: &str,
    body: &[u8],
    now: u64,
) -> Result<String> {
    let encoded = header
        .strip_prefix("Nostr ")
        .context("not a NIP-98 Authorization header")?;
    let json = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("not base64")?;
    let event: Event = serde_json::from_slice(&json).context("not a nostr event")?;
    ensure!(
        event.kind == HTTP_AUTH_KIND,
        "not a NIP-98 event (kind {})",
        event.kind
    );
    ensure!(
        event.created_at.abs_diff(now) <= MAX_CLOCK_SKEW_SECS,
        "the event is too old, or from the future: check your clock"
    );
    ensure!(
        event.tag("u") == Some(url),
        "the event was signed for another URL"
    );
    ensure!(
        event
            .tag("method")
            .is_some_and(|m| m.eq_ignore_ascii_case(method)),
        "the event was signed for another method"
    );
    if !body.is_empty() {
        let hash = hex::encode(Sha256::digest(body));
        ensure!(
            event.tag("payload") == Some(hash.as_str()),
            "the event does not cover this body"
        );
    }
    event.verify()?;
    Ok(event.pubkey.to_ascii_lowercase())
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    hex::decode(s)
        .ok()
        .and_then(|b| b.try_into().ok())
        .context("not 32 bytes of hex")
}

/// An `npub1…` or a 64-character hex pubkey, as hex. Checks it is a point.
pub fn parse_pubkey(input: &str) -> Result<String> {
    let input = input.trim();
    let bytes = if input.starts_with("npub1") {
        let (hrp, data) = bech32::decode(input).context("not a valid npub")?;
        if hrp.as_str() != "npub" {
            bail!("not an npub");
        }
        data.try_into().ok().context("an npub holds 32 bytes")?
    } else {
        hex32(input).context("expected an npub1… or a hex pubkey")?
    };
    XOnlyPublicKey::from_slice(&bytes).context("not a valid public key")?;
    Ok(hex::encode(bytes))
}

pub fn npub(pubkey_hex: &str) -> String {
    let bytes = hex::decode(pubkey_hex).unwrap_or_default();
    bech32::encode::<Bech32>(Hrp::parse_unchecked("npub"), &bytes).unwrap_or_default()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use secp256k1::Keypair;

    /// A signed event, the way a NIP-07 extension makes one.
    pub fn sign(
        secret: [u8; 32],
        created_at: u64,
        kind: u64,
        tags: Vec<Vec<String>>,
        content: &str,
    ) -> serde_json::Value {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_seckey_slice(&secp, &secret).unwrap();
        let mut event = Event {
            id: String::new(),
            pubkey: hex::encode(keypair.x_only_public_key().0.serialize()),
            created_at,
            kind,
            tags,
            content: content.into(),
            sig: String::new(),
        };
        let id = event.computed_id();
        event.id = hex::encode(id);
        event.sig = hex::encode(
            secp.sign_schnorr_no_aux_rand(&Message::from_digest(id), &keypair)
                .serialize(),
        );
        serde_json::json!({
            "id": event.id, "pubkey": event.pubkey, "created_at": event.created_at, "kind": event.kind,
            "tags": event.tags, "content": event.content, "sig": event.sig,
        })
    }

    pub fn auth_header(secret: [u8; 32], url: &str, body: &[u8], created_at: u64) -> String {
        let tags = vec![
            vec!["u".into(), url.into()],
            vec!["method".into(), "POST".into()],
            vec!["payload".into(), hex::encode(Sha256::digest(body))],
        ];
        let event = sign(secret, created_at, HTTP_AUTH_KIND, tags, "");
        format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(event.to_string())
        )
    }

    const URL: &str = "https://run.lnurlcash.com/api/beta";

    #[test]
    fn a_nip98_header_verifies() {
        let body = br#"{"note":"a mint for my meetup"}"#;
        let header = auth_header([3; 32], URL, body, 1_000_000);
        let pubkey = verify_http_auth(&header, URL, "POST", body, 1_000_030).unwrap();
        assert_eq!(pubkey.len(), 64);
    }

    #[test]
    fn a_nip98_header_is_bound_to_url_body_and_time() {
        let body = br#"{"note":"x"}"#;
        let header = auth_header([3; 32], URL, body, 1_000_000);
        assert!(
            verify_http_auth(
                &header,
                "https://evil.example/api/beta",
                "POST",
                body,
                1_000_000
            )
            .is_err()
        );
        assert!(verify_http_auth(&header, URL, "POST", br#"{"note":"y"}"#, 1_000_000).is_err());
        assert!(verify_http_auth(&header, URL, "GET", body, 1_000_000).is_err());
        assert!(verify_http_auth(&header, URL, "POST", body, 1_000_000 + 61).is_err());
    }

    #[test]
    fn a_tampered_event_is_refused() {
        let mut event = sign([3; 32], 5, 1, vec![], "hello");
        let ok: Event = serde_json::from_value(event.clone()).unwrap();
        ok.verify().unwrap();
        event["content"] = "hellO".into();
        let bad: Event = serde_json::from_value(event).unwrap();
        assert!(bad.verify().is_err());
    }

    /// NIP-01 escaping: newlines, quotes and unicode survive the id.
    #[test]
    fn ids_cover_escaped_content() {
        let event: Event =
            serde_json::from_value(sign([4; 32], 5, 1, vec![], "line\n\"quoted\" ⚡ \\ /"))
                .unwrap();
        event.verify().unwrap();
    }

    #[test]
    fn npubs_round_trip() {
        // NIP-19's own example
        let hex = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        assert_eq!(parse_pubkey(npub).unwrap(), hex);
        assert_eq!(super::npub(hex), npub);
        assert_eq!(parse_pubkey(hex).unwrap(), hex);
        assert!(parse_pubkey("npub1garbage").is_err());
        assert!(parse_pubkey(&"ff".repeat(32)).is_err());
    }
}
