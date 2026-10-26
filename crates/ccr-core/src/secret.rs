//! Encryption at rest for stored OAuth logins and backups, the same on every platform.
//!
//! Each blob is XChaCha20-Poly1305 under a random 256-bit data key, with a fresh random 24-byte
//! nonce: `CCR2 || nonce || ciphertext+tag`. The associated data names what the blob is for (a
//! given account's login, or a backup), so a blob moved to another row or file fails to open.
//!
//! The key lives in `<data dir>/master.key`, created on first use, readable only by the owner.
//! Reading it never prompts, which the statusline needs: it runs every few seconds per session,
//! often in headless or SSH sessions. The key file protects the store against casual reads and
//! against a copy of the store leaking on its own; it does not protect against malware running
//! as the same user, which could read Claude Code's own plaintext `.credentials.json` anyway.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

/// Every sealed blob starts with these bytes, so the format can change later.
pub const MAGIC: &[u8; 4] = b"CCR2";
pub const KEY_FILE: &str = "master.key";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// Associated data for an account's stored login.
pub fn credential_aad(account_id: i64) -> Vec<u8> {
    format!("cc-rotate/credential/v2/account:{account_id}").into_bytes()
}

/// Associated data for a daily backup of the profile's credentials.
pub const BACKUP_AAD: &[u8] = b"cc-rotate/backup/v2";

pub fn is_sealed(blob: &[u8]) -> bool {
    blob.starts_with(MAGIC)
}

/// The data key, held in memory only while a store is open.
pub struct Key(XChaCha20Poly1305);

impl Key {
    pub fn from_bytes(bytes: &[u8]) -> Result<Key> {
        if bytes.len() != KEY_LEN {
            bail!("the encryption key must be {KEY_LEN} bytes");
        }
        let cipher = XChaCha20Poly1305::new_from_slice(bytes)
            .map_err(|_| anyhow!("invalid encryption key"))?;
        Ok(Key(cipher))
    }

    /// A random key that is never written anywhere (for in-memory stores).
    pub fn ephemeral() -> Result<Key> {
        Key::from_bytes(&random::<KEY_LEN>()?)
    }

    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = random::<NONCE_LEN>()?;
        let ciphertext = self
            .0
            .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let Some(body) = sealed.strip_prefix(MAGIC.as_slice()) else {
            bail!("not a cc-rotate encrypted blob");
        };
        let Some((nonce, ciphertext)) = body.split_first_chunk::<NONCE_LEN>() else {
            bail!("the encrypted blob is truncated");
        };
        self.0
            .decrypt(
                &XNonce::from(*nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                anyhow!("the encrypted blob does not match the key or was changed on disk")
            })
    }
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("the system random generator failed: {e}"))?;
    Ok(bytes)
}

pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE)
}

/// Read the key file. `Ok(None)` when it does not exist yet.
pub fn read_key(path: &Path) -> Result<Option<Key>> {
    match std::fs::read(path) {
        Ok(bytes) => Key::from_bytes(&bytes)
            .with_context(|| format!("{} is not a valid key file", path.display()))
            .map(Some),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Read the key file, creating it first if it does not exist.
///
/// Many processes may get here at once on first use, so the new key is written in full to a
/// temporary file and then published with a hard link, which fails instead of replacing an
/// existing file. Whoever loses the race reads the winner's key, so every process ends up
/// with the same one.
pub fn load_or_create_key(path: &Path) -> Result<Key> {
    if let Some(key) = read_key(path)? {
        return Ok(key);
    }
    let dir = path
        .parent()
        .context("the key file has no parent directory")?;
    create_private_dir(dir)?;
    let bytes = random::<KEY_LEN>()?;
    let tmp = dir.join(format!(".{KEY_FILE}.{}.tmp", uuid::Uuid::new_v4().simple()));
    crate::credfile::write_new_private(&tmp, &bytes)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    }
    read_key(path)?.with_context(|| format!("{} vanished while it was created", path.display()))
}

/// Create a directory (and its parents) that only its owner can enter on Unix.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Make `path` reachable by its owner alone (`mode`, 0700 for a folder or 0600 for a file) when
/// it is this user's and allows others more. A folder created by an older version, or one
/// `CC_ROTATE_DATA_DIR` names, may have been made with the default mode.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path, mode: u32) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() == me && meta.permissions().mode() & 0o077 != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
pub fn restrict_to_owner(_path: &Path, _mode: u32) {}

/// Whether the key file is readable by anyone but its owner (Unix permission bits).
#[cfg(unix)]
pub fn key_file_exposed(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).ok()?.permissions().mode();
    Some(mode & 0o077 != 0)
}

#[cfg(not(unix))]
pub fn key_file_exposed(_path: &Path) -> Option<bool> {
    None
}

/// Windows DPAPI, kept only to read logins and backups written by cc-rotate before the store
/// moved to the platform-independent format above.
#[cfg(windows)]
pub mod dpapi {
    use std::ptr;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
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

    /// Seal like cc-rotate did before, so tests can build a store from that time.
    #[cfg(test)]
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
        use windows_sys::Win32::Security::Cryptography::CryptProtectData;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data folder or store an older version made with the default mode is made owner-only.
    #[cfg(unix)]
    #[test]
    fn the_store_and_its_folder_are_made_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let db = dir.join("store.db");
        std::fs::write(&db, b"").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        crate::Store::open(&db).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn round_trip() {
        let key = Key::ephemeral().unwrap();
        let plain = br#"{"accessToken":"a","refreshToken":"r"}"#;
        let sealed = key.seal(plain, &credential_aad(1)).unwrap();
        assert!(is_sealed(&sealed));
        assert!(!sealed.windows(plain.len()).any(|w| w == plain));
        assert_eq!(key.open(&sealed, &credential_aad(1)).unwrap(), plain);
    }

    #[test]
    fn nonces_are_fresh() {
        let key = Key::ephemeral().unwrap();
        let a = key.seal(b"same", BACKUP_AAD).unwrap();
        let b = key.seal(b"same", BACKUP_AAD).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tampering_is_detected() {
        let key = Key::ephemeral().unwrap();
        let sealed = key.seal(b"secret", BACKUP_AAD).unwrap();
        for i in [MAGIC.len(), MAGIC.len() + NONCE_LEN, sealed.len() - 1] {
            let mut bad = sealed.clone();
            bad[i] ^= 0x01;
            assert!(key.open(&bad, BACKUP_AAD).is_err(), "byte {i} flipped");
        }
        assert!(key.open(&sealed[..sealed.len() - 1], BACKUP_AAD).is_err());
        assert!(key.open(&sealed[..10], BACKUP_AAD).is_err());
    }

    #[test]
    fn blob_for_another_account_is_rejected() {
        let key = Key::ephemeral().unwrap();
        let sealed = key.seal(b"login of 1", &credential_aad(1)).unwrap();
        assert!(key.open(&sealed, &credential_aad(2)).is_err());
        assert!(key.open(&sealed, BACKUP_AAD).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let sealed = Key::ephemeral()
            .unwrap()
            .seal(b"secret", BACKUP_AAD)
            .unwrap();
        assert!(Key::ephemeral().unwrap().open(&sealed, BACKUP_AAD).is_err());
    }

    #[test]
    fn key_file_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir.path().join("data"));
        assert!(read_key(&path).unwrap().is_none());
        let first = load_or_create_key(&path).unwrap();
        let sealed = first.seal(b"x", BACKUP_AAD).unwrap();
        let again = load_or_create_key(&path).unwrap();
        assert_eq!(again.open(&sealed, BACKUP_AAD).unwrap(), b"x");
        assert_eq!(std::fs::read(&path).unwrap().len(), KEY_LEN);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != KEY_FILE)
            .collect();
        assert!(leftovers.is_empty(), "no temporary files left behind");
    }

    #[test]
    fn concurrent_first_use_agrees_on_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let sealed: Vec<Vec<u8>> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let key = load_or_create_key(&path).unwrap();
                    key.seal(b"same key", BACKUP_AAD).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        let key = read_key(&path).unwrap().unwrap();
        for blob in sealed {
            assert_eq!(key.open(&blob, BACKUP_AAD).unwrap(), b"same key");
        }
    }

    #[test]
    fn corrupt_key_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());
        std::fs::write(&path, b"short").unwrap();
        assert!(load_or_create_key(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_and_data_dir_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let path = key_path(&data);
        load_or_create_key(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&data), 0o700);
        assert_eq!(key_file_exposed(&path), Some(false));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(key_file_exposed(&path), Some(true));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_blobs_still_open() {
        let sealed = dpapi::protect(b"legacy").unwrap();
        assert_eq!(dpapi::unprotect(&sealed).unwrap(), b"legacy");
    }
}
