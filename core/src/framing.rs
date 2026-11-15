//! 4-byte big-endian length-prefixed framing used on every Nearby socket.

/// Frames larger than this are treated as a protocol error (Nearby caps at ~5 MB).
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

pub fn encode(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + msg.len());
    out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    out.extend_from_slice(msg);
    out
}

pub fn decode_len(header: [u8; 4]) -> crate::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return crate::proto_err(format!("frame too large: {len}"));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let f = super::encode(b"hello");
        assert_eq!(&f[..4], &[0, 0, 0, 5]);
        assert_eq!(super::decode_len(f[..4].try_into().unwrap()).unwrap(), 5);
        assert!(super::decode_len([0x7f, 0, 0, 0]).is_err());
    }
}
