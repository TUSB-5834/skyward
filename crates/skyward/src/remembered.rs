//! The screen lock kept between starts, so that the decrypt page can open the
//! data partition without it being typed again.
//!
//! The file is encrypted with a software key derived from boot properties.
//! Those properties are public, so this is convenience storage rather than
//! hardware-backed protection of the password. The option starts off.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Where the credential is kept: a file of the recovery's own beside the
/// settings, for the same reasons that file is there. `/metadata` needs no
/// screen lock to open and is there before the data partition is.
pub const FILE: &str = "/metadata/skyward/credential";

/// The layout of the file: its magic, then the nonce and the sealed
/// credential.
const MAGIC: &[u8] = b"SKYC1";

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

/// An upper bound on the credential kept by this recovery.
const MOST: usize = 256;
const MOST_FILE: usize = MAGIC.len() + NONCE_BYTES + TAG_BYTES + MOST;

/// Derive the software key from the serial reported by the bootloader.
/// Anyone who knows that serial and this format can decrypt the saved file.
fn sealing_key(identity: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"skyward remembered credential v1");
    h.update(identity);
    h.finalize().into()
}

/// The phone's own identity, as much of it as a copy of the file would not
/// carry with it: the serial the bootloader reports.
fn identity() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for name in ["ro.serialno", "ro.boot.serialno"] {
        if let Some(value) = skyward_sys::prop(name).filter(|value| !value.is_empty()) {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(b'\n');
        }
    }
    if bytes.is_empty() {
        bail!("the phone has no serial to identify remembered credentials");
    }
    Ok(bytes)
}

/// Seal `credential` and write it where a later start finds it.
///
/// `/metadata` is kept read-only by the recovery and made writable only for
/// the length of a write, as the settings file is: a plain `fs::write` here
/// fails with a read-only filesystem and the credential is silently never
/// kept, which is what happened before this took the mount's mode into
/// account.
pub fn save(credential: &str) -> Result<()> {
    if credential.is_empty() || credential.len() > MOST {
        bail!("a credential of {} bytes is not one to keep", credential.len());
    }
    let file = seal(credential, &identity()?)?;
    skyward_vab::metadata_writable(true).context("make /metadata writable")?;
    let wrote = write_file(Path::new(FILE), &file);
    let back = skyward_vab::metadata_writable(false).context("make /metadata read-only again");
    if let Err(error) = wrote {
        return Err(match back {
            Err(back) => error.context(format!("{back:#}")),
            Ok(()) => error,
        });
    }
    back
}

fn seal(credential: &str, identity: &[u8]) -> Result<Vec<u8>> {
    if credential.is_empty() || credential.len() > MOST {
        bail!("a credential of {} bytes is not one to keep", credential.len());
    }
    let key = sealing_key(identity);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let sealed = cipher.encrypt(&nonce, credential.as_bytes()).map_err(|_| anyhow::anyhow!("seal the credential"))?;
    let mut file = Vec::with_capacity(MAGIC.len() + NONCE_BYTES + sealed.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(nonce.as_slice());
    file.extend_from_slice(&sealed);
    Ok(file)
}

/// The sealed credential into its file, flushed, then moved into place.
fn write_file(path: &Path, sealed: &[u8]) -> Result<()> {
    let writing = path.with_extension("new");
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder).with_context(|| format!("make {}", folder.display()))?;
    }
    match fs::remove_file(&writing) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e).context("remove the unfinished credential"),
    }
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&writing)?;
    file.write_all(sealed).with_context(|| format!("write {}", writing.display()))?;
    // Flushed and moved into place, so a start cut off in the middle leaves
    // either the old credential or the new one and never half of either.
    file.sync_all().with_context(|| format!("flush {}", writing.display()))?;
    fs::rename(&writing, path).with_context(|| format!("move {} over {}", writing.display(), path.display()))?;
    if let Some(folder) = path.parent() {
        fs::File::open(folder)?.sync_all().with_context(|| format!("flush {}", folder.display()))?;
    }
    Ok(())
}

/// The credential kept from an earlier start, where one is kept and opens.
///
/// A file that does not open is not an error worth failing anything over: it
/// is a phone that was wiped, or a credential sealed by a phone whose identity
/// changed. It is taken away, so that the next start does not read it again.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn load() -> Option<String> {
    let file = read_file(Path::new(FILE)).ok()?;
    match open(&file, &identity().ok()?) {
        Ok(credential) => Some(credential),
        Err(_) => {
            let _ = forget();
            None
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MOST_FILE as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn open(file: &[u8], identity: &[u8]) -> Result<String> {
    if file.len() < MAGIC.len() + NONCE_BYTES + TAG_BYTES || file.len() > MOST_FILE || !file.starts_with(MAGIC) {
        bail!("invalid remembered credential record");
    }
    let mut opened = &file[MAGIC.len()..];
    let nonce = Nonce::from_slice(&opened[..NONCE_BYTES]);
    opened = &opened[NONCE_BYTES..];
    let key = sealing_key(identity);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let plain = cipher.decrypt(nonce, opened).map_err(|_| anyhow::anyhow!("open the credential"))?;
    if plain.is_empty() || plain.len() > MOST {
        bail!("invalid remembered credential length");
    }
    String::from_utf8(plain).context("read the credential text")
}

/// Take the kept credential away, where one is kept.
///
/// The file lives on a mount the recovery keeps read-only, so it is made
/// writable for the removal alone. A file that is not there is what this
/// wanted anyway.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn forget() -> Result<()> {
    if !Path::new(FILE).exists() {
        return Ok(());
    }
    skyward_vab::metadata_writable(true).context("make /metadata writable")?;
    let removed = match fs::remove_file(FILE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {FILE}")),
    };
    let back = skyward_vab::metadata_writable(false).context("make /metadata read-only again");
    if let Err(error) = removed {
        return Err(match back {
            Err(back) => error.context(format!("{back:#}")),
            Ok(()) => error,
        });
    }
    back
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_sealed_on_one_phone_does_not_open_on_another() {
        let here = b"ro.serialno=abc123\n";
        let elsewhere = b"ro.serialno=def456\n";
        let file = seal("1234", here).unwrap();
        assert_eq!(open(&file, here).unwrap(), "1234");
        assert!(open(&file, elsewhere).is_err(), "a copy taken to another phone opens nothing");
    }

    #[test]
    fn what_was_sealed_does_not_read_as_the_credential_itself() {
        let credential = "a password that cannot fit in a nonce";
        let file = seal(credential, b"serial=x").unwrap();
        assert!(!file.windows(credential.len()).any(|w| w == credential.as_bytes()), "the plaintext is not in the file");
    }

    #[test]
    fn a_file_that_is_not_one_or_is_damaged_opens_nothing() {
        assert!(open(b"", b"s").is_err());
        assert!(open(b"not a sealed credential at all", b"s").is_err());
        let mut file = seal("1234", b"s").unwrap();
        let last = file.len() - 1;
        file[last] ^= 1;
        assert!(open(&file, b"s").is_err(), "a damaged seal opens nothing");
    }

    #[test]
    fn every_write_seals_under_a_fresh_nonce() {
        let (a, b) = (seal("1234", b"s").unwrap(), seal("1234", b"s").unwrap());
        assert_ne!(a, b, "the same credential is not written as the same bytes twice");
    }

    #[test]
    fn a_credential_that_is_empty_or_absurd_is_not_kept() {
        assert!(save("").is_err());
        assert!(save(&"1".repeat(MOST + 1)).is_err());
    }

    #[test]
    fn replacing_a_credential_keeps_it_private_and_complete() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let sealed = seal("1234", b"s").unwrap();
        write_file(&path, &sealed).unwrap();
        assert_eq!(open(&read_file(&path).unwrap(), b"s").unwrap(), "1234");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!path.with_extension("new").exists());
    }

    #[test]
    fn an_oversized_file_is_read_only_up_to_the_limit_and_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        fs::write(&path, vec![0; MOST_FILE * 20]).unwrap();
        let bytes = read_file(&path).unwrap();
        assert_eq!(bytes.len(), MOST_FILE + 1);
        assert!(open(&bytes, b"s").is_err());
    }
}
