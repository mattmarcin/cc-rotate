//! Encryption at rest for stored OAuth logins.
//!
//! On Windows this is DPAPI with the CurrentUser scope, so only the same Windows user on the
//! same machine can decrypt the store. Other platforms are not supported yet.

use anyhow::Result;

#[cfg(windows)]
mod imp {
    use std::ptr;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    const ENTROPY: &[u8] = b"cc-rotate/oauth/v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with LocalAlloc.
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            bytes
        }
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
        let input = blob(plain);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: all pointers reference live buffers for the duration of the call.
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            bail!(
                "CryptProtectData failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(unsafe { take(out) })
    }

    pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>> {
        let input = blob(sealed);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        };
        // SAFETY: all pointers reference live buffers for the duration of the call.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            bail!(
                "CryptUnprotectData failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(unsafe { take(out) })
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};

    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>> {
        bail!("encrypted credential storage is only implemented on Windows so far")
    }

    pub fn unprotect(_sealed: &[u8]) -> Result<Vec<u8>> {
        bail!("encrypted credential storage is only implemented on Windows so far")
    }
}

pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
    imp::protect(plain)
}

pub fn unprotect(sealed: &[u8]) -> Result<Vec<u8>> {
    imp::unprotect(sealed)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let plain = br#"{"accessToken":"a","refreshToken":"r"}"#;
        let sealed = protect(plain).unwrap();
        assert_ne!(&sealed[..], &plain[..]);
        assert_eq!(unprotect(&sealed).unwrap(), plain);
    }

    #[test]
    fn tampered_blob_fails() {
        let mut sealed = protect(b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(unprotect(&sealed).is_err());
    }
}
