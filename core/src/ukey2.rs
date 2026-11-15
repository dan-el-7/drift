//! UKEY2 handshake (P256_SHA512, next protocol AES_256_CBC-HMAC_SHA256) and
//! the Quick Share key schedule. See google/ukey2 README and NOTES.md.

use hkdf::Hkdf;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, PublicKey};
use prost::Message;
use rand::RngCore;
use sha2::{Digest, Sha256, Sha512};

use crate::proto::securegcm as gcm;
use crate::proto::securemessage as sm;
use crate::{Error, Result, proto_err};

pub const NEXT_PROTOCOL: &str = "AES_256_CBC-HMAC_SHA256";
const VERSION: i32 = 1;
const P256_SHA512: i32 = gcm::Ukey2HandshakeCipher::P256Sha512 as i32;

/// SHA256("D2D")
pub const D2D_SALT: [u8; 32] = hex32("82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510");
/// SHA256("SecureMessage")
pub const SECURE_MESSAGE_SALT: [u8; 32] =
    hex32("BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E");

const fn hex32(s: &str) -> [u8; 32] {
    const fn nib(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'F' => c - b'A' + 10,
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex"),
        }
    }
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}

fn hkdf32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

// ---------- public key encoding (Java BigInteger, two's complement) ----------

fn to_bigint_bytes(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    if v.first().is_some_and(|x| x & 0x80 != 0) {
        v.insert(0, 0);
    }
    v
}

fn from_bigint_bytes(b: &[u8]) -> Result<[u8; 32]> {
    let start = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    let trimmed = &b[start..];
    if trimmed.len() > 32 {
        return Err(Error::Crypto("EC coordinate longer than 32 bytes"));
    }
    let mut out = [0u8; 32];
    out[32 - trimmed.len()..].copy_from_slice(trimmed);
    Ok(out)
}

pub fn encode_public_key(pk: &PublicKey) -> Vec<u8> {
    let p = pk.to_encoded_point(false);
    sm::GenericPublicKey {
        r#type: sm::PublicKeyType::EcP256 as i32,
        ec_p256_public_key: Some(sm::EcP256PublicKey {
            x: to_bigint_bytes(p.x().expect("uncompressed")),
            y: to_bigint_bytes(p.y().expect("uncompressed")),
        }),
        rsa2048_public_key: None,
        dh2048_public_key: None,
    }
    .encode_to_vec()
}

pub fn decode_public_key(b: &[u8]) -> Result<PublicKey> {
    let g = sm::GenericPublicKey::decode(b)?;
    let ec = g.ec_p256_public_key.ok_or(Error::Crypto("missing EC P-256 key"))?;
    let x = from_bigint_bytes(&ec.x)?;
    let y = from_bigint_bytes(&ec.y)?;
    let point = EncodedPoint::from_affine_coordinates(&x.into(), &y.into(), false);
    Option::from(PublicKey::from_encoded_point(&point)).ok_or(Error::Crypto("point not on curve"))
}

fn ukey2_wrap(t: gcm::ukey2_message::Type, data: Vec<u8>) -> Vec<u8> {
    gcm::Ukey2Message { message_type: Some(t as i32), message_data: Some(data) }.encode_to_vec()
}

fn ukey2_unwrap(raw: &[u8], want: gcm::ukey2_message::Type) -> Result<Vec<u8>> {
    let m = gcm::Ukey2Message::decode(raw)?;
    if m.message_type != Some(want as i32) {
        return proto_err(format!("expected UKEY2 {want:?}, got type {:?}", m.message_type));
    }
    Ok(m.message_data.unwrap_or_default())
}

fn dhs(secret: &EphemeralSecret, peer: &PublicKey) -> [u8; 32] {
    let shared = secret.diffie_hellman(peer);
    Sha256::digest(shared.raw_secret_bytes()).into()
}

fn random32() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut r);
    r
}

// ---------- handshake results ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

#[derive(Clone)]
pub struct HandshakeResult {
    pub auth_string: [u8; 32],
    pub next_secret: [u8; 32],
}

impl HandshakeResult {
    fn derive(dhs: &[u8; 32], client_init: &[u8], server_init: &[u8]) -> Self {
        let mut info = Vec::with_capacity(client_init.len() + server_init.len());
        info.extend_from_slice(client_init);
        info.extend_from_slice(server_init);
        Self {
            auth_string: hkdf32(b"UKEY2 v1 auth", dhs, &info),
            next_secret: hkdf32(b"UKEY2 v1 next", dhs, &info),
        }
    }

    /// The 4-digit PIN Quick Share shows on both devices.
    pub fn pin(&self) -> String {
        pin_from_auth_string(&self.auth_string)
    }

    pub fn session_keys(&self, role: Role) -> SessionKeys {
        let d2d_client = hkdf32(&D2D_SALT, &self.next_secret, b"client");
        let d2d_server = hkdf32(&D2D_SALT, &self.next_secret, b"server");
        let (mine, theirs) = match role {
            Role::Server => (d2d_server, d2d_client),
            Role::Client => (d2d_client, d2d_server),
        };
        SessionKeys {
            encrypt: hkdf32(&SECURE_MESSAGE_SALT, &mine, b"ENC:2"),
            send_hmac: hkdf32(&SECURE_MESSAGE_SALT, &mine, b"SIG:1"),
            decrypt: hkdf32(&SECURE_MESSAGE_SALT, &theirs, b"ENC:2"),
            recv_hmac: hkdf32(&SECURE_MESSAGE_SALT, &theirs, b"SIG:1"),
        }
    }
}

pub fn pin_from_auth_string(auth: &[u8]) -> String {
    const MODULO: i32 = 9973;
    const MULT: i32 = 31;
    let mut hash: i32 = 0;
    let mut mult: i32 = 1;
    for &b in auth {
        hash = (hash + (b as i8 as i32) * mult) % MODULO;
        mult = (mult * MULT) % MODULO;
    }
    format!("{:04}", hash.abs())
}

#[derive(Clone)]
pub struct SessionKeys {
    pub encrypt: [u8; 32],
    pub send_hmac: [u8; 32],
    pub decrypt: [u8; 32],
    pub recv_hmac: [u8; 32],
}

// ---------- server side ----------

pub struct ServerHandshake {
    secret: EphemeralSecret,
    client_init_raw: Vec<u8>,
    server_init_raw: Vec<u8>,
    commitment: Vec<u8>,
}

impl ServerHandshake {
    /// Process the client's `Ukey2Message{CLIENT_INIT}` (raw, without length
    /// prefix). Returns the handshake state and the raw `SERVER_INIT` to send.
    pub fn handle_client_init(client_init_raw: &[u8]) -> Result<(Self, Vec<u8>)> {
        let ci = gcm::Ukey2ClientInit::decode(
            ukey2_unwrap(client_init_raw, gcm::ukey2_message::Type::ClientInit)?.as_slice(),
        )?;
        if ci.version != Some(VERSION) {
            return proto_err(format!("UKEY2 version {:?}", ci.version));
        }
        if ci.random.as_ref().map(Vec::len) != Some(32) {
            return proto_err("UKEY2 client random must be 32 bytes");
        }
        if ci.next_protocol.as_deref() != Some(NEXT_PROTOCOL) {
            return proto_err(format!("UKEY2 next protocol {:?}", ci.next_protocol));
        }
        let commitment = ci
            .cipher_commitments
            .iter()
            .find(|c| c.handshake_cipher == Some(P256_SHA512))
            .and_then(|c| c.commitment.clone())
            .ok_or(Error::Crypto("client offered no P256_SHA512 commitment"))?;

        let secret = EphemeralSecret::random(&mut rand::thread_rng());
        let si = gcm::Ukey2ServerInit {
            version: Some(VERSION),
            random: Some(random32()),
            handshake_cipher: Some(P256_SHA512),
            public_key: Some(encode_public_key(&secret.public_key())),
        };
        let server_init_raw = ukey2_wrap(gcm::ukey2_message::Type::ServerInit, si.encode_to_vec());
        Ok((
            Self {
                secret,
                client_init_raw: client_init_raw.to_vec(),
                server_init_raw: server_init_raw.clone(),
                commitment,
            },
            server_init_raw,
        ))
    }

    pub fn handle_client_finish(self, client_finish_raw: &[u8]) -> Result<HandshakeResult> {
        let digest = Sha512::digest(client_finish_raw);
        if digest[..] != self.commitment[..] {
            return Err(Error::Crypto("ClientFinished does not match commitment"));
        }
        let cf = gcm::Ukey2ClientFinished::decode(
            ukey2_unwrap(client_finish_raw, gcm::ukey2_message::Type::ClientFinish)?.as_slice(),
        )?;
        let peer = decode_public_key(&cf.public_key.unwrap_or_default())?;
        Ok(HandshakeResult::derive(
            &dhs(&self.secret, &peer),
            &self.client_init_raw,
            &self.server_init_raw,
        ))
    }
}

// ---------- client side ----------

pub struct ClientHandshake {
    secret: EphemeralSecret,
    client_init_raw: Vec<u8>,
    client_finish_raw: Vec<u8>,
}

impl ClientHandshake {
    /// Returns the handshake state and the raw `CLIENT_INIT` to send.
    pub fn new() -> (Self, Vec<u8>) {
        let secret = EphemeralSecret::random(&mut rand::thread_rng());
        let cf = gcm::Ukey2ClientFinished { public_key: Some(encode_public_key(&secret.public_key())) };
        let client_finish_raw = ukey2_wrap(gcm::ukey2_message::Type::ClientFinish, cf.encode_to_vec());
        let ci = gcm::Ukey2ClientInit {
            version: Some(VERSION),
            random: Some(random32()),
            cipher_commitments: vec![gcm::ukey2_client_init::CipherCommitment {
                handshake_cipher: Some(P256_SHA512),
                commitment: Some(Sha512::digest(&client_finish_raw).to_vec()),
            }],
            next_protocol: Some(NEXT_PROTOCOL.into()),
        };
        let client_init_raw = ukey2_wrap(gcm::ukey2_message::Type::ClientInit, ci.encode_to_vec());
        (Self { secret, client_init_raw: client_init_raw.clone(), client_finish_raw }, client_init_raw)
    }

    /// Process `SERVER_INIT`; returns the result and the raw `CLIENT_FINISH` to send.
    pub fn handle_server_init(self, server_init_raw: &[u8]) -> Result<(HandshakeResult, Vec<u8>)> {
        let si = gcm::Ukey2ServerInit::decode(
            ukey2_unwrap(server_init_raw, gcm::ukey2_message::Type::ServerInit)?.as_slice(),
        )?;
        if si.version != Some(VERSION) || si.handshake_cipher != Some(P256_SHA512) {
            return proto_err("unsupported UKEY2 server init");
        }
        let peer = decode_public_key(&si.public_key.unwrap_or_default())?;
        let r = HandshakeResult::derive(&dhs(&self.secret, &peer), &self.client_init_raw, server_init_raw);
        Ok((r, self.client_finish_raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salts_are_sha256_of_labels() {
        assert_eq!(D2D_SALT.as_slice(), Sha256::digest(b"D2D").as_slice());
        assert_eq!(SECURE_MESSAGE_SALT.as_slice(), Sha256::digest(b"SecureMessage").as_slice());
    }

    #[test]
    fn rfc5869_case1_hkdf_sha256() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let mut okm = [0u8; 42];
        Hkdf::<Sha256>::new(Some(&salt), &ikm).expand(&info, &mut okm).unwrap();
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn bigint_coordinate_encoding() {
        let mut x = [0u8; 32];
        x[0] = 0x80;
        let enc = to_bigint_bytes(&x);
        assert_eq!(enc.len(), 33);
        assert_eq!(from_bigint_bytes(&enc).unwrap(), x);
        // Short (leading zeros dropped) coordinates are left-padded.
        assert_eq!(from_bigint_bytes(&[1, 2]).unwrap()[30..], [1, 2]);
    }

    #[test]
    fn client_server_agree() {
        let (client, ci) = ClientHandshake::new();
        let (server, si) = ServerHandshake::handle_client_init(&ci).unwrap();
        let (cr, cf) = client.handle_server_init(&si).unwrap();
        let sr = server.handle_client_finish(&cf).unwrap();
        assert_eq!(cr.auth_string, sr.auth_string);
        assert_eq!(cr.next_secret, sr.next_secret);
        assert_eq!(cr.pin(), sr.pin());
        let ck = cr.session_keys(Role::Client);
        let sk = sr.session_keys(Role::Server);
        assert_eq!(ck.encrypt, sk.decrypt);
        assert_eq!(ck.send_hmac, sk.recv_hmac);
        assert_eq!(sk.encrypt, ck.decrypt);
    }

    #[test]
    fn tampered_client_finish_rejected() {
        let (_client, ci) = ClientHandshake::new();
        let (server, _si) = ServerHandshake::handle_client_init(&ci).unwrap();
        let (other, _) = ClientHandshake::new();
        assert!(server.handle_client_finish(&other.client_finish_raw).is_err());
    }

    #[test]
    fn pin_vectors() {
        assert_eq!(pin_from_auth_string(&[0u8; 32]), "0000");
        // byte 1 at index 0 only -> hash 1
        let mut a = [0u8; 32];
        a[0] = 1;
        assert_eq!(pin_from_auth_string(&a), "0001");
        // negative i8 bytes: 0xFF = -1 at index 1 -> -31 -> "0031"
        let mut b = [0u8; 32];
        b[1] = 0xFF;
        assert_eq!(pin_from_auth_string(&b), "0031");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
