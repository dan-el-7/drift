//! D2D SecureMessage channel: AES-256-CBC + HMAC-SHA256 with per-direction
//! sequence numbers. Each sealed message carries one serialized OfflineFrame.

use aes::Aes256;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use prost::Message;
use rand::RngCore;
use sha2::Sha256;

use crate::proto::securegcm as gcm;
use crate::proto::securemessage as sm;
use crate::ukey2::SessionKeys;
use crate::{Error, Result, proto_err};

type HmacSha256 = Hmac<Sha256>;

pub struct Sealer {
    key: [u8; 32],
    hmac: [u8; 32],
    seq: i32,
}

pub struct Opener {
    key: [u8; 32],
    hmac: [u8; 32],
    seq: i32,
}

pub fn split(keys: &SessionKeys) -> (Sealer, Opener) {
    (
        Sealer { key: keys.encrypt, hmac: keys.send_hmac, seq: 0 },
        Opener { key: keys.decrypt, hmac: keys.recv_hmac, seq: 0 },
    )
}

fn d2d_metadata() -> Vec<u8> {
    gcm::GcmMetadata { r#type: gcm::Type::DeviceToDeviceMessage as i32, version: Some(1) }.encode_to_vec()
}

impl Sealer {
    /// Wrap a serialized OfflineFrame into a serialized SecureMessage.
    pub fn seal(&mut self, offline_frame: &[u8]) -> Vec<u8> {
        self.seq += 1;
        let d2d = gcm::DeviceToDeviceMessage {
            message: Some(offline_frame.to_vec()),
            sequence_number: Some(self.seq),
        }
        .encode_to_vec();
        let mut iv = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut iv);
        let body = cbc::Encryptor::<Aes256>::new(&self.key.into(), &iv.into())
            .encrypt_padded_vec_mut::<Pkcs7>(&d2d);
        let hb = sm::HeaderAndBody {
            header: sm::Header {
                signature_scheme: sm::SigScheme::HmacSha256 as i32,
                encryption_scheme: sm::EncScheme::Aes256Cbc as i32,
                verification_key_id: None,
                decryption_key_id: None,
                iv: Some(iv.to_vec()),
                public_metadata: Some(d2d_metadata()),
                associated_data_length: None,
            },
            body,
        }
        .encode_to_vec();
        let mut mac = HmacSha256::new_from_slice(&self.hmac).expect("any key length");
        mac.update(&hb);
        sm::SecureMessage { header_and_body: hb, signature: mac.finalize().into_bytes().to_vec() }
            .encode_to_vec()
    }
}

impl Opener {
    /// Verify and decrypt a serialized SecureMessage; returns the OfflineFrame bytes.
    pub fn open(&mut self, secure_message: &[u8]) -> Result<Vec<u8>> {
        let m = sm::SecureMessage::decode(secure_message)?;
        let mut mac = HmacSha256::new_from_slice(&self.hmac).expect("any key length");
        mac.update(&m.header_and_body);
        mac.verify_slice(&m.signature).map_err(|_| Error::Crypto("bad HMAC"))?;
        let hb = sm::HeaderAndBody::decode(m.header_and_body.as_slice())?;
        let iv: [u8; 16] = hb
            .header
            .iv
            .as_deref()
            .and_then(|v| v.try_into().ok())
            .ok_or(Error::Crypto("missing/short IV"))?;
        let plain = cbc::Decryptor::<Aes256>::new(&self.key.into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&hb.body)
            .map_err(|_| Error::Crypto("bad padding"))?;
        let d2d = gcm::DeviceToDeviceMessage::decode(plain.as_slice())?;
        let seq = d2d.sequence_number.unwrap_or(0);
        if seq != self.seq + 1 {
            return proto_err(format!("sequence {seq}, expected {}", self.seq + 1));
        }
        self.seq = seq;
        Ok(d2d.message.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ukey2::{ClientHandshake, Role, ServerHandshake};

    fn pair() -> ((Sealer, Opener), (Sealer, Opener)) {
        let (c, ci) = ClientHandshake::new();
        let (s, si) = ServerHandshake::handle_client_init(&ci).unwrap();
        let (cr, cf) = c.handle_server_init(&si).unwrap();
        let sr = s.handle_client_finish(&cf).unwrap();
        (split(&cr.session_keys(Role::Client)), split(&sr.session_keys(Role::Server)))
    }

    #[test]
    fn both_directions_and_sequence() {
        let ((mut cs, mut co), (mut ss, mut so)) = pair();
        for i in 0..3u8 {
            let m = cs.seal(&[i; 40]);
            assert_eq!(so.open(&m).unwrap(), vec![i; 40]);
        }
        let m = ss.seal(b"hi");
        assert_eq!(co.open(&m).unwrap(), b"hi");
    }

    #[test]
    fn replay_and_tamper_rejected() {
        let ((mut cs, _), (_, mut so)) = pair();
        let m = cs.seal(b"x");
        so.open(&m).unwrap();
        assert!(so.open(&m).is_err(), "replay must fail sequence check");
        let mut t = cs.seal(b"y");
        let n = t.len();
        t[n - 1] ^= 1;
        assert!(so.open(&t).is_err(), "tampered signature must fail");
    }
}
