//! Drift-to-Drift trust: pair once (with the 4-digit code you already compare), then skip the prompt.
//!
//! Each Drift install has a long-term P-256 device key. Inside Quick Share's PairedKeyEncryption
//! frame (the slot Google fills with account certificates) a Drift peer puts:
//!   signed_data          = ECDSA-P256 (r‖s, 64 bytes) over SHA-256("drift-trust-v1" ‖ auth ‖ role)
//!   secret_id_hash       = key id: the first 6 bytes of SHA-256(public key, SEC1 uncompressed)
//!   optional_signed_data = the public key (65 bytes, SEC1 uncompressed)
//! `auth` is the UKEY2 auth string of this connection (the 4-digit code comes from it), so a
//! signature proves possession of the key *in this session* and can't be replayed elsewhere. `role`
//! (0 receiver, 1 sender) stops a signature being reflected back at its signer.
//!
//! First time: the user compares the code and ticks "Trust this device"; the app stores the peer's
//! public key (it was proven in a session the user just authenticated). Every later session: a valid
//! signature from a stored key = a trusted device. Stock Quick Share sends random bytes here and
//! never sees ours (we only send our key to peers whose endpoint info carries the Drift tag).

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::proto::sharing as sh;

pub const ROLE_RECEIVER: u8 = 0;
pub const ROLE_SENDER: u8 = 1;
const DOMAIN: &[u8] = b"drift-trust-v1";

/// This install's long-term identity.
pub struct DeviceKey {
    signing: SigningKey,
}

impl std::fmt::Debug for DeviceKey {
    // never print the secret
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.key_id().iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

impl DeviceKey {
    pub fn generate() -> Self {
        Self { signing: SigningKey::random(&mut rand::rngs::OsRng) }
    }

    /// Restore from the 32-byte secret (stored protected by the platform).
    pub fn from_secret(bytes: &[u8]) -> Option<Self> {
        SigningKey::from_slice(bytes).ok().map(|signing| Self { signing })
    }

    pub fn secret(&self) -> Vec<u8> {
        self.signing.to_bytes().to_vec()
    }

    pub fn public(&self) -> Vec<u8> {
        self.signing.verifying_key().to_encoded_point(false).as_bytes().to_vec()
    }

    pub fn key_id(&self) -> [u8; 6] {
        key_id(&self.public())
    }

    /// Our PairedKeyEncryption frame for a Drift peer in this session.
    pub fn frame(&self, auth: &[u8; 32], our_role: u8) -> sh::PairedKeyEncryptionFrame {
        let sig: Signature = self.signing.sign(&message(auth, our_role));
        sh::PairedKeyEncryptionFrame {
            signed_data: Some(sig.to_bytes().to_vec()),
            secret_id_hash: Some(self.key_id().to_vec()),
            optional_signed_data: Some(self.public()),
            ..Default::default()
        }
    }
}

/// A short presence hint for Drift adverts: only a device that already trusts `public` can
/// recognise it, and the salt changes every advertising session, so it can't be used to follow
/// the device around.
pub fn presence_hint(public: &[u8], salt: [u8; 2]) -> [u8; 4] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"drift-hint-v1");
    h.update(public);
    h.update(salt);
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

pub fn key_id(public: &[u8]) -> [u8; 6] {
    let h = Sha256::digest(public);
    h[..6].try_into().unwrap()
}

fn message(auth: &[u8; 32], role: u8) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + 33);
    m.extend_from_slice(DOMAIN);
    m.extend_from_slice(auth);
    m.push(role);
    m
}

/// A peer's device key, proven in this session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerKey {
    pub id: [u8; 6],
    /// SEC1 uncompressed public key.
    pub public: Vec<u8>,
}

impl PeerKey {
    pub fn id_hex(&self) -> String {
        self.id.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Check a peer's PairedKeyEncryption frame. Some(key) only if it carries a public key, the key id
/// matches it, and the signature over this session's auth string (with the peer's role) verifies.
/// Stock Quick Share (random bytes, no public key) gives None.
pub fn verify(frame: &sh::PairedKeyEncryptionFrame, auth: &[u8; 32], peer_role: u8) -> Option<PeerKey> {
    let public = frame.optional_signed_data.as_deref()?;
    let id = key_id(public);
    if frame.secret_id_hash.as_deref() != Some(&id[..]) {
        return None;
    }
    let vk = VerifyingKey::from_sec1_bytes(public).ok()?;
    let sig = Signature::from_slice(frame.signed_data.as_deref()?).ok()?;
    vk.verify(&message(auth, peer_role), &sig).ok()?;
    Some(PeerKey { id, public: public.to_vec() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn presence_hint_needs_the_key_and_changes_with_salt() {
        let k = DeviceKey::generate();
        let other = DeviceKey::generate();
        let h = presence_hint(&k.public(), [1, 2]);
        assert_eq!(h, presence_hint(&k.public(), [1, 2]));
        assert_ne!(h, presence_hint(&k.public(), [1, 3]));
        assert_ne!(h, presence_hint(&other.public(), [1, 2]));
    }

    use super::*;

    #[test]
    fn a_signed_frame_verifies_for_its_session_and_role_only() {
        let k = DeviceKey::generate();
        let auth = [7u8; 32];
        let f = k.frame(&auth, ROLE_SENDER);
        let p = verify(&f, &auth, ROLE_SENDER).expect("verifies");
        assert_eq!(p.id, k.key_id());
        assert_eq!(p.public, k.public());
        assert!(verify(&f, &[8u8; 32], ROLE_SENDER).is_none(), "another session");
        assert!(verify(&f, &auth, ROLE_RECEIVER).is_none(), "reflected back");
    }

    #[test]
    fn stock_quick_share_frames_and_tampering_are_rejected() {
        let auth = [1u8; 32];
        let stock = sh::PairedKeyEncryptionFrame { signed_data: Some(vec![9; 72]), secret_id_hash: Some(vec![3; 6]), ..Default::default() };
        assert!(verify(&stock, &auth, ROLE_SENDER).is_none());
        let k = DeviceKey::generate();
        let other = DeviceKey::generate();
        let mut f = k.frame(&auth, ROLE_SENDER);
        f.optional_signed_data = Some(other.public()); // swapped key
        assert!(verify(&f, &auth, ROLE_SENDER).is_none());
    }

    #[test]
    fn keys_round_trip_through_their_secret() {
        let k = DeviceKey::generate();
        let r = DeviceKey::from_secret(&k.secret()).unwrap();
        assert_eq!(r.public(), k.public());
    }
}
