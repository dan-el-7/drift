//! BLE advertisement encoding for Nearby Connections (from google/nearby
//! `connections/implementation/ble_advertisement.cc` and
//! `mediums/ble/ble_advertisement.cc`), plus the Bluetooth Classic service UUID.

use rand::RngCore;
use sha2::{Digest, Sha256};

/// 16-bit service UUID ("Copresence") whose service data carries the advertisement.
pub const COPRESENCE_UUID16: u16 = 0xFEF3;
/// Legacy (non-extended) advertising allows at most this many service-data bytes after the UUID.
pub const MAX_LEGACY_SERVICE_DATA: usize = 27;
pub const MAX_FAST_ENDPOINT_INFO: usize = 17;
pub const MAX_ENDPOINT_INFO: usize = 131;

const PCP_V1: u8 = 0x23; // version 1 << 5 | PCP 3 (P2P_POINT_TO_POINT)
const MEDIUM_VERSION: u8 = 2;
const SOCKET_VERSION: u8 = 2;

pub fn service_id_hash(service_id: &str) -> [u8; 3] {
    let h = Sha256::digest(service_id.as_bytes());
    [h[0], h[1], h[2]]
}

fn device_token() -> [u8; 2] {
    let mut t = [0u8; 2];
    rand::thread_rng().fill_bytes(&mut t);
    t
}

/// "Fast" advertisement: fits a legacy BLE advertisement, but endpoint info is
/// capped at 17 bytes (no device name). Bluetooth address is taken from the
/// BLE advertiser address by the phone.
pub fn fast_advertisement(endpoint_id: &[u8; 4], endpoint_info: &[u8]) -> crate::Result<Vec<u8>> {
    if endpoint_info.len() > MAX_FAST_ENDPOINT_INFO {
        return crate::proto_err(format!("fast endpoint info {} > 17 bytes", endpoint_info.len()));
    }
    let mut pcp = vec![PCP_V1];
    pcp.extend_from_slice(endpoint_id);
    pcp.push(endpoint_info.len() as u8);
    pcp.extend_from_slice(endpoint_info);

    let mut out = vec![(MEDIUM_VERSION << 5) | (SOCKET_VERSION << 2) | (1 << 1)];
    out.push(pcp.len() as u8);
    out.extend_from_slice(&pcp);
    out.extend_from_slice(&device_token());
    debug_assert!(out.len() <= MAX_LEGACY_SERVICE_DATA);
    Ok(out)
}

/// Regular advertisement (needs BLE extended advertising): carries a full
/// endpoint info (with device name) and the Bluetooth Classic MAC.
/// `bt_mac` is in display order (most significant byte first).
pub fn regular_advertisement(
    service_id: &str,
    endpoint_id: &[u8; 4],
    endpoint_info: &[u8],
    bt_mac: [u8; 6],
) -> crate::Result<Vec<u8>> {
    if endpoint_info.len() > MAX_ENDPOINT_INFO {
        return crate::proto_err("endpoint info too long");
    }
    let hash = service_id_hash(service_id);
    let mut pcp = vec![PCP_V1];
    pcp.extend_from_slice(&hash);
    pcp.extend_from_slice(endpoint_id);
    pcp.push(endpoint_info.len() as u8);
    pcp.extend_from_slice(endpoint_info);
    pcp.extend_from_slice(&bt_mac);
    pcp.push(0); // UWB address length
    pcp.push(0); // extra field: WebRTC not connectable

    let mut out = vec![(MEDIUM_VERSION << 5) | (SOCKET_VERSION << 2)];
    out.extend_from_slice(&hash);
    out.extend_from_slice(&(pcp.len() as u32).to_be_bytes());
    out.extend_from_slice(&pcp);
    out.extend_from_slice(&device_token());
    Ok(out)
}

/// Java `UUID.nameUUIDFromBytes` (MD5, version 3), used for the RFCOMM service.
pub fn name_uuid(name: &str) -> [u8; 16] {
    use md5::{Digest as _, Md5};
    let mut b: [u8; 16] = Md5::digest(name.as_bytes()).into();
    b[6] = (b[6] & 0x0f) | 0x30;
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

pub fn uuid_string(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_matches_google_capture() {
        // Captured from the phone log: data=[0x23 'U' 'L' 'B' 'Y' 0x11 <17 bytes>]
        let info = [0x16, 0x59, 0xe5, 0x6a, 0xe6, 0xbd, 0x03, 0x79, 0xa1, 0xb5, 0x81, 0x82, 0xcd, 0x18, 0x8d, 0x65, 0xcc];
        let adv = fast_advertisement(b"ULBY", &info).unwrap();
        assert_eq!(adv[0], 0x4A, "version 2, socket 2, fast");
        assert_eq!(adv[1], 23);
        assert_eq!(&adv[2..8], &[0x23, b'U', b'L', b'B', b'Y', 0x11]);
        assert_eq!(adv.len(), MAX_LEGACY_SERVICE_DATA);
    }

    #[test]
    fn regular_layout() {
        let adv = regular_advertisement("NearbySharing", b"ABCD", &[0; 20], [1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(adv[0], 0x48);
        assert_eq!(&adv[1..4], &[0xFC, 0x9F, 0x5E]);
        let len = u32::from_be_bytes(adv[4..8].try_into().unwrap()) as usize;
        assert_eq!(len, 1 + 3 + 4 + 1 + 20 + 6 + 1 + 1);
        assert_eq!(adv.len(), 8 + len + 2);
    }

    #[test]
    fn java_name_uuid() {
        // UUID.nameUUIDFromBytes("hello".getBytes()) == 5d41402a-bc4b-3a76-b971-9d911017c592
        assert_eq!(uuid_string(&name_uuid("hello")), "5d41402a-bc4b-3a76-b971-9d911017c592");
    }
}

// ---------------------------------------------------------------------------
// Discoverer side (sending): wake beacon + decoding what a receiver advertises.
// ---------------------------------------------------------------------------

/// 16-bit UUID of the Nearby Share "fast initiation" wake beacon.
pub const FAST_INIT_UUID16: u16 = 0xFE2C;
const FAST_INIT_MODEL_ID: [u8; 3] = [0xFC, 0x12, 0x8E];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastInitType {
    /// Receivers may show a "device nearby is sharing" notification.
    Notify = 0,
    Silent = 1,
}

/// Service data (after the UUID) of the wake beacon a sender broadcasts so that
/// nearby Android receivers start advertising. Layout (google/nearby
/// `fast_init_ble_beacon.h`): model id[3] | meta (version:3 type:3 uwb:1 cert:1)
/// | tx power | uwb metadata[1] | uwb address[8] | salt[1] | secret id hash[8]
/// | flags (require_bt_advertising:1 self_only:1 unused:6). 24 bytes.
pub fn fast_init_beacon(kind: FastInitType, require_bt_advertising: bool) -> Vec<u8> {
    let mut out = FAST_INIT_MODEL_ID.to_vec();
    out.push(((kind as u8) & 0b111) << 2); // version 0, no UWB, no sender cert
    out.push(0); // adjusted tx power
    out.extend_from_slice(&[0u8; 1 + 8]); // uwb metadata + address
    let mut salt_hash = [0u8; 9];
    rand::thread_rng().fill_bytes(&mut salt_hash);
    out.extend_from_slice(&salt_hash);
    out.push(if require_bt_advertising { 0x80 } else { 0 });
    out
}

/// A receiver found by the discoverer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub endpoint_id: [u8; 4],
    pub endpoint_info: Vec<u8>,
    /// Bluetooth Classic address (MSB first) when the advertisement carries one.
    pub bt_mac: Option<[u8; 6]>,
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> crate::Result<&'a [u8]> {
    if b.len() < n {
        return crate::proto_err("advertisement truncated");
    }
    let (h, t) = b.split_at(n);
    *b = t;
    Ok(h)
}

/// Decode `0xFEF3` service data (medium-level BLE advertisement, fast or regular).
pub fn parse_advertisement(data: &[u8]) -> crate::Result<Discovered> {
    let mut b = data;
    let version = take(&mut b, 1)?[0];
    let fast = version & 0b10 != 0;
    let pcp = if fast {
        let len = take(&mut b, 1)?[0] as usize;
        take(&mut b, len)?
    } else {
        take(&mut b, 3)?; // service id hash
        let len = u32::from_be_bytes(take(&mut b, 4)?.try_into().unwrap()) as usize;
        take(&mut b, len)?
    };
    let mut p = pcp;
    if take(&mut p, 1)?[0] != PCP_V1 {
        return crate::proto_err("not a P2P_POINT_TO_POINT v1 advertisement");
    }
    if !fast {
        take(&mut p, 3)?;
    }
    let endpoint_id: [u8; 4] = take(&mut p, 4)?.try_into().unwrap();
    let info_len = take(&mut p, 1)?[0] as usize;
    let endpoint_info = take(&mut p, info_len)?.to_vec();
    let bt_mac = if fast {
        None
    } else {
        let m: [u8; 6] = take(&mut p, 6)?.try_into().unwrap();
        (m != [0; 6]).then_some(m)
    };
    Ok(Discovered { endpoint_id, endpoint_info, bt_mac })
}

/// Decode a Nearby "Bluetooth device name" (base64 of PCP | endpoint id |
/// service id hash | flags | reserved[6] | info len | endpoint info | [uwb]).
pub fn parse_bluetooth_name(name: &str) -> crate::Result<Discovered> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let raw = [URL_SAFE_NO_PAD, URL_SAFE, STANDARD_NO_PAD, STANDARD]
        .iter()
        .find_map(|e| e.decode(name.trim()).ok())
        .ok_or(crate::Error::Protocol("not base64".into()))?;
    let mut p = raw.as_slice();
    if take(&mut p, 1)?[0] != PCP_V1 {
        return crate::proto_err("not a Nearby device name");
    }
    let endpoint_id: [u8; 4] = take(&mut p, 4)?.try_into().unwrap();
    take(&mut p, 3)?; // service id hash
    take(&mut p, 1)?; // flags
    take(&mut p, 6)?; // reserved
    let info_len = take(&mut p, 1)?[0] as usize;
    let endpoint_info = take(&mut p, info_len)?.to_vec();
    Ok(Discovered { endpoint_id, endpoint_info, bt_mac: None })
}

#[cfg(test)]
mod discover_tests {
    use super::*;

    #[test]
    fn beacon_layout() {
        let b = fast_init_beacon(FastInitType::Silent, true);
        assert_eq!(b.len(), 24);
        assert_eq!(&b[..3], &[0xFC, 0x12, 0x8E]);
        assert_eq!(b[3], 1 << 2);
        assert_eq!(b[23], 0x80);
    }

    #[test]
    fn parses_our_own_advertisements() {
        let info = [7u8; 20];
        let adv = regular_advertisement("NearbySharing", b"ABCD", &info, [1, 2, 3, 4, 5, 6]).unwrap();
        let d = parse_advertisement(&adv).unwrap();
        assert_eq!(&d.endpoint_id, b"ABCD");
        assert_eq!(d.endpoint_info, info);
        assert_eq!(d.bt_mac, Some([1, 2, 3, 4, 5, 6]));
        let fast = fast_advertisement(b"WXYZ", &[9u8; 17]).unwrap();
        let d = parse_advertisement(&fast).unwrap();
        assert_eq!(&d.endpoint_id, b"WXYZ");
        assert_eq!(d.bt_mac, None);
    }

    #[test]
    fn parses_captured_google_fast_advert() {
        // From the phone log: BleAdvertisement { version=2, socketVersion=2, isFast=true, data=[0x23 'U' 'L' 'B' 'Y' 0x11 ...] }
        let mut adv = vec![0x4A, 23, 0x23, b'U', b'L', b'B', b'Y', 0x11];
        adv.extend_from_slice(&[0x16, 0x59, 0xe5, 0x6a, 0xe6, 0xbd, 0x03, 0x79, 0xa1, 0xb5, 0x81, 0x82, 0xcd, 0x18, 0x8d, 0x65, 0xcc]);
        adv.extend_from_slice(&[0, 0]);
        let d = parse_advertisement(&adv).unwrap();
        assert_eq!(&d.endpoint_id, b"ULBY");
        assert_eq!(d.endpoint_info.len(), 17);
    }

    #[test]
    fn bluetooth_name_roundtrip() {
        use base64::Engine;
        let mut raw = vec![0x23];
        raw.extend_from_slice(b"QRST");
        raw.extend_from_slice(&[0xFC, 0x9F, 0x5E, 0, 0, 0, 0, 0, 0, 0, 3, 1, 2, 3]);
        let name = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
        let d = parse_bluetooth_name(&name).unwrap();
        assert_eq!(&d.endpoint_id, b"QRST");
        assert_eq!(d.endpoint_info, vec![1, 2, 3]);
        assert!(parse_bluetooth_name("Dan's Galaxy Buds").is_err());
    }
}
