//! The trustless roadmap snapshot (ADR-0006 §5, ADR-0007): a canonical JSON document of the public roadmaps, signed as a
//! Sui **personal message** so anyone can verify it with standard Sui tooling.
//!
//! - **Canonical JSON:** keys sorted at every level, no whitespace, UTF-8, integers only. The same data therefore gives
//!   the same bytes and the same SHA-256, whatever order a map was built in.
//! - **Signature:** the signed bytes are the intent `[3, 0, 0]` (scope PersonalMessage, version 0, app Sui) followed by the
//!   BCS encoding of the message as `vector<u8>` (a ULEB128 length, then the bytes); the digest is BLAKE2b-256 of that;
//!   an Ed25519 signature is serialised `0x00 ‖ signature(64) ‖ public key(32)` and base64-encoded; the address is
//!   BLAKE2b-256(`0x00 ‖ public key`), `0x` and 64 hex digits. `@mysten/sui`'s `verifyPersonalMessageSignature` and
//!   `sui keytool` check exactly this (the committed fixture is verified with `@mysten/sui` in CI).
//!
//! Only Ed25519 is produced and accepted: it is the scheme of the snapshot signer (ADR-0007).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use blake2::digest::consts::U32;
use blake2::{Blake2b, Digest};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::Value;
use sha2::Sha256;

/// The document's `schema` value.
pub const SCHEMA: &str = "content-hub.roadmap-snapshot/1";
/// The largest document that is built, signed and stored (1 MiB).
pub const MAX_BYTES: usize = 1 << 20;

const ED25519_FLAG: u8 = 0x00;
const PERSONAL_MESSAGE_INTENT: [u8; 3] = [3, 0, 0];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("the document is {0} bytes; at most {MAX_BYTES} are allowed")]
    TooLarge(usize),
    #[error("the document holds a non-integer number, which has no canonical form")]
    NotCanonical,
    #[error("the signer seed is not a valid Ed25519 seed")]
    BadSeed,
    #[error("not a Sui Ed25519 signature: {0}")]
    BadSignature(&'static str),
    #[error("the signature does not match the message")]
    Mismatch,
}

fn blake2b_256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2b::<U32>::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// Canonical JSON bytes of a document (sorted keys, compact, integers only), at most [`MAX_BYTES`].
pub fn canonical(doc: &Value) -> Result<Vec<u8>, SnapshotError> {
    let mut out = Vec::new();
    write(doc, &mut out)?;
    if out.len() > MAX_BYTES {
        return Err(SnapshotError::TooLarge(out.len()));
    }
    Ok(out)
}

fn write(v: &Value, out: &mut Vec<u8>) -> Result<(), SnapshotError> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => {
            if !(n.is_u64() || n.is_i64()) {
                return Err(SnapshotError::NotCanonical);
            }
            out.extend_from_slice(n.to_string().as_bytes());
        }
        Value::String(s) => {
            out.extend_from_slice(serde_json::to_string(s).map_err(|_| SnapshotError::NotCanonical)?.as_bytes())
        }
        Value::Array(a) => {
            out.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write(x, out)?;
            }
            out.push(b']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(k).map_err(|_| SnapshotError::NotCanonical)?.as_bytes());
                out.push(b':');
                write(&m[k], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The address of an Ed25519 public key.
pub fn address_of(public_key: &[u8; 32]) -> String {
    let h = blake2b_256(&[&[ED25519_FLAG], public_key]);
    format!("0x{}", h.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

fn uleb128(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// The 32-byte digest a Sui personal-message signature signs.
pub fn personal_message_digest(message: &[u8]) -> [u8; 32] {
    let mut intent_message = Vec::with_capacity(message.len() + 8);
    intent_message.extend_from_slice(&PERSONAL_MESSAGE_INTENT);
    uleb128(message.len(), &mut intent_message);
    intent_message.extend_from_slice(message);
    blake2b_256(&[&intent_message])
}

/// An Ed25519 signer for personal messages (the snapshot signer; ADR-0007).
pub struct Signer {
    key: Ed25519KeyPair,
    public: [u8; 32],
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signer({})", self.address())
    }
}

impl Signer {
    /// From a 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Signer, SnapshotError> {
        let key = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| SnapshotError::BadSeed)?;
        let public: [u8; 32] = key.public_key().as_ref().try_into().map_err(|_| SnapshotError::BadSeed)?;
        Ok(Signer { key, public })
    }

    /// From the base64 of a 32-byte seed (what `SNAPSHOT_SIGNER_SEED` holds).
    pub fn from_base64_seed(text: &str) -> Result<Signer, SnapshotError> {
        let bytes = STANDARD.decode(text.trim()).map_err(|_| SnapshotError::BadSeed)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| SnapshotError::BadSeed)?;
        Signer::from_seed(&seed)
    }

    pub fn address(&self) -> String {
        address_of(&self.public)
    }

    /// The serialised signature (`0x00 ‖ signature ‖ public key`, base64) of a personal message.
    pub fn sign_personal_message(&self, message: &[u8]) -> String {
        let digest = personal_message_digest(message);
        let sig = self.key.sign(&digest);
        let mut out = Vec::with_capacity(97);
        out.push(ED25519_FLAG);
        out.extend_from_slice(sig.as_ref());
        out.extend_from_slice(&self.public);
        STANDARD.encode(out)
    }
}

/// Check a serialised Ed25519 personal-message signature; returns the signing address.
pub fn verify_personal_message(message: &[u8], signature_b64: &str) -> Result<String, SnapshotError> {
    let raw = STANDARD.decode(signature_b64.trim()).map_err(|_| SnapshotError::BadSignature("not base64"))?;
    if raw.len() != 97 {
        return Err(SnapshotError::BadSignature("an Ed25519 signature is 97 bytes"));
    }
    if raw[0] != ED25519_FLAG {
        return Err(SnapshotError::BadSignature("only the Ed25519 scheme (flag 0x00) is accepted"));
    }
    let public: [u8; 32] = raw[65..97].try_into().map_err(|_| SnapshotError::BadSignature("length"))?;
    UnparsedPublicKey::new(&ED25519, public)
        .verify(&personal_message_digest(message), &raw[1..65])
        .map_err(|_| SnapshotError::Mismatch)?;
    Ok(address_of(&public))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signer() -> Signer {
        Signer::from_seed(&[7u8; 32]).expect("seed")
    }

    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        let a = json!({"b": [1, {"y": true, "x": null}], "a": "é\"\n", "c": -5});
        let b: Value = serde_json::from_str(r#"{"c":-5,"a":"é\"\n","b":[1,{"x":null,"y":true}]}"#).expect("json");
        assert_eq!(canonical(&a).expect("a"), canonical(&b).expect("b"));
        assert_eq!(
            String::from_utf8(canonical(&a).expect("a")).expect("utf8"),
            r#"{"a":"é\"\n","b":[1,{"x":null,"y":true}],"c":-5}"#
        );
    }

    #[test]
    fn canonical_json_refuses_what_has_no_canonical_form_or_is_too_big() {
        assert_eq!(canonical(&json!({"x": 1.5})), Err(SnapshotError::NotCanonical));
        let big = json!({"x": "a".repeat(MAX_BYTES)});
        assert!(matches!(canonical(&big), Err(SnapshotError::TooLarge(_))));
    }

    #[test]
    fn a_signature_verifies_and_names_its_signer() {
        let s = signer();
        let msg = br#"{"schema":"x"}"#;
        let sig = s.sign_personal_message(msg);
        assert_eq!(verify_personal_message(msg, &sig), Ok(s.address()));
        assert_eq!(s.address().len(), 66);
        assert!(s.address().starts_with("0x"));
        // Ed25519 is deterministic: the same message gives the same signature.
        assert_eq!(sig, s.sign_personal_message(msg));
    }

    #[test]
    fn tampering_is_detected() {
        let s = signer();
        let sig = s.sign_personal_message(b"hello");
        assert_eq!(verify_personal_message(b"hellp", &sig), Err(SnapshotError::Mismatch));
        let mut raw = STANDARD.decode(&sig).expect("b64");
        raw[10] ^= 1;
        assert_eq!(verify_personal_message(b"hello", &STANDARD.encode(&raw)), Err(SnapshotError::Mismatch));
        raw[10] ^= 1;
        raw[0] = 0x01; // secp256k1's flag: not accepted
        assert!(matches!(
            verify_personal_message(b"hello", &STANDARD.encode(&raw)),
            Err(SnapshotError::BadSignature(_))
        ));
        for bad in ["", "not base64!!", &STANDARD.encode([0u8; 10])] {
            assert!(matches!(verify_personal_message(b"hello", bad), Err(SnapshotError::BadSignature(_))), "{bad}");
        }
        // Another key's signature names another address.
        let other = Signer::from_seed(&[8u8; 32]).expect("seed");
        assert_ne!(verify_personal_message(b"hello", &other.sign_personal_message(b"hello")), Ok(s.address()));
    }

    #[test]
    fn the_digest_includes_the_intent_and_a_length_prefix() {
        // A message and the same message with the intent bytes prepended must not collide.
        assert_ne!(personal_message_digest(b"abc"), personal_message_digest(&[&[3, 0, 0][..], b"abc"].concat()));
        // ULEB128: 300 is 0xAC 0x02.
        let mut v = Vec::new();
        uleb128(300, &mut v);
        assert_eq!(v, [0xac, 0x02]);
        let mut v = Vec::new();
        uleb128(5, &mut v);
        assert_eq!(v, [5]);
    }

    #[test]
    fn seeds_parse_from_base64_only_at_the_right_length() {
        let ok = STANDARD.encode([7u8; 32]);
        assert_eq!(Signer::from_base64_seed(&ok).expect("seed").address(), signer().address());
        for bad in ["", "AAAA", &STANDARD.encode([1u8; 31]), &STANDARD.encode([1u8; 33]), "%%%"] {
            assert!(Signer::from_base64_seed(bad).is_err(), "{bad}");
        }
        assert!(!format!("{:?}", signer()).contains(&ok));
    }

    #[test]
    fn sha256_is_hex() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}

#[cfg(test)]
mod fixture_tests {
    use crate::snapshot as snap;
    include!("../fixtures/snapshot_fixture.rs");

    #[test]
    fn the_committed_fixture_is_what_the_code_produces() {
        // Regenerate with `cargo run -p content-hub-core --example gen-snapshot-fixture > tests/contract/snapshot-fixture.json`.
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/contract/snapshot-fixture.json"))
                .expect("fixture parses");
        assert_eq!(committed, snapshot_fixture());
    }

    #[test]
    fn the_fixture_verifies_with_the_rust_verifier() {
        use base64::Engine;
        let f = snapshot_fixture();
        let message =
            base64::engine::general_purpose::STANDARD.decode(f["message_base64"].as_str().expect("m")).expect("b64");
        assert!(message.len() > 127, "the length prefix needs two ULEB128 bytes");
        assert_eq!(
            super::verify_personal_message(&message, f["signature"].as_str().expect("sig")).as_deref(),
            Ok(f["address"].as_str().expect("addr"))
        );
    }
}
