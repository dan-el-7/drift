//! DPAPI: encrypt small secrets (Drift's device key) to the current Windows user.

use anyhow::{Result, bail};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(Some(HLOCAL(out.pbData as _))) };
    v
}

/// Encrypt to this Windows user (readable only by them, on this PC).
pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe { CryptProtectData(&blob(data), None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }?;
    if out.pbData.is_null() {
        bail!("CryptProtectData returned nothing");
    }
    Ok(take(out))
}

pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe { CryptUnprotectData(&blob(data), None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out) }?;
    if out.pbData.is_null() {
        bail!("CryptUnprotectData returned nothing");
    }
    Ok(take(out))
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let secret = b"drift device key bytes";
        let p = super::protect(secret).unwrap();
        assert_ne!(&p[..], &secret[..]);
        assert_eq!(super::unprotect(&p).unwrap(), secret);
    }
}
