//! Endpoint identity: mDNS service name and the "endpoint info" blob.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;

/// mDNS service type for Quick Share. First 6 bytes of SHA256("NearbySharing").
pub const SERVICE_TYPE: &str = "_FC9F5ED42C8A._tcp.local.";
/// First 3 bytes of SHA256("NearbySharing").
pub const SERVICE_ID_HASH: [u8; 3] = [0xFC, 0x9F, 0x5E];
const PCP: u8 = 0x23;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DeviceType {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
}

impl DeviceType {
    fn from_bits(b: u8) -> Self {
        match b {
            1 => Self::Phone,
            2 => Self::Tablet,
            3 => Self::Laptop,
            _ => Self::Unknown,
        }
    }
}

/// Random 4-character alphanumeric endpoint ID.
pub fn random_endpoint_id() -> [u8; 4] {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    std::array::from_fn(|_| ALNUM[rng.gen_range(0..ALNUM.len())])
}

/// mDNS instance name: base64url(0x23 | endpoint_id[4] | service_id_hash[3] | 0x00 0x00).
pub fn mdns_instance_name(endpoint_id: &[u8; 4]) -> String {
    let mut b = Vec::with_capacity(10);
    b.push(PCP);
    b.extend_from_slice(endpoint_id);
    b.extend_from_slice(&SERVICE_ID_HASH);
    b.extend_from_slice(&[0, 0]);
    URL_SAFE_NO_PAD.encode(b)
}

/// Drift's tag in the 16 metadata bytes (random salt + key to stock Quick Share in Everyone mode, which
/// it doesn't interpret), then our protocol version. Lets each side tell Drift from stock Quick Share.
const DRIFT_TAG: [u8; 3] = [0xD7, 0x1F, 0x7A];
pub const DRIFT_PROTOCOL: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    pub device_type: DeviceType,
    pub visible: bool,
    /// 2 bytes salt + 14 bytes encrypted metadata key. Random for us.
    pub metadata: [u8; 16],
    pub name: Option<String>,
}

impl EndpointInfo {
    pub fn new_visible(name: &str, device_type: DeviceType) -> Self {
        let mut metadata = [0u8; 16];
        rand::thread_rng().fill(&mut metadata);
        Self { device_type, visible: true, metadata, name: Some(name.to_owned()) }
    }

    /// Visible, and tagged as Drift (the rest of the metadata stays random).
    pub fn new_drift(name: &str, device_type: DeviceType) -> Self {
        let mut s = Self::new_visible(name, device_type);
        s.metadata[..3].copy_from_slice(&DRIFT_TAG);
        s.metadata[3] = DRIFT_PROTOCOL;
        s
    }

    /// Carry a presence hint (trust::presence_hint) so devices that trust us can recognise us
    /// without a name or address (Android can't advertise its Bluetooth address).
    pub fn with_hint(mut self, salt: [u8; 2], hint: [u8; 4]) -> Self {
        self.metadata[4..6].copy_from_slice(&salt);
        self.metadata[6..10].copy_from_slice(&hint);
        self
    }

    /// (salt, hint) if this is a Drift endpoint. Without a hint these bytes are random.
    pub fn drift_hint(&self) -> Option<([u8; 2], [u8; 4])> {
        self.drift_version()?;
        Some((self.metadata[4..6].try_into().unwrap(), self.metadata[6..10].try_into().unwrap()))
    }

    /// Some(protocol version) if this endpoint is Drift.
    pub fn drift_version(&self) -> Option<u8> {
        (self.metadata[..3] == DRIFT_TAG).then_some(self.metadata[3])
    }

    pub fn encode(&self) -> Vec<u8> {
        // bits: vvv h ttt r  (version 0, hidden bit, type, reserved)
        let hidden = if self.visible { 0 } else { 1 };
        let mut out = vec![(hidden << 4) | ((self.device_type as u8) << 1)];
        out.extend_from_slice(&self.metadata);
        if self.visible {
            let name = self.name.as_deref().unwrap_or("");
            // Name length is one byte; truncate on a char boundary.
            let mut end = name.len().min(255);
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            out.push(end as u8);
            out.extend_from_slice(&name.as_bytes()[..end]);
        }
        out
    }

    pub fn decode(b: &[u8]) -> crate::Result<Self> {
        if b.len() < 17 {
            return crate::proto_err("endpoint info too short");
        }
        let device_type = DeviceType::from_bits((b[0] >> 1) & 0b111);
        let visible = (b[0] >> 4) & 1 == 0;
        let metadata: [u8; 16] = b[1..17].try_into().unwrap();
        let name = if visible && b.len() > 17 {
            let len = b[17] as usize;
            let end = (18 + len).min(b.len());
            Some(String::from_utf8_lossy(&b[18..end]).into_owned())
        } else {
            None
        };
        Ok(Self { device_type, visible, metadata, name })
    }

    pub fn to_txt(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_layout() {
        let n = mdns_instance_name(b"ABCD");
        let raw = URL_SAFE_NO_PAD.decode(n).unwrap();
        assert_eq!(raw, [0x23, b'A', b'B', b'C', b'D', 0xFC, 0x9F, 0x5E, 0, 0]);
    }

    #[test]
    fn endpoint_info_roundtrip() {
        let e = EndpointInfo::new_visible("Drift PC", DeviceType::Laptop);
        let b = e.encode();
        assert_eq!(b[0], 3 << 1);
        assert_eq!(EndpointInfo::decode(&b).unwrap(), e);
    }

    #[test]
    fn service_hash_matches_sha256() {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(b"NearbySharing");
        assert_eq!(&h[..3], &SERVICE_ID_HASH);
        assert_eq!(hex_upper(&h[..6]), "FC9F5ED42C8A");
    }

    fn hex_upper(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }
}
