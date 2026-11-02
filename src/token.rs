//! The loopback API's bearer token: 32 random bytes, hex-encoded, in
//! `<data_dir>/api-token` (mode 0600). Created on first use, reused after.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// `<data_dir>/api-token`.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("api-token")
}

/// Read the token, creating the file (0600) when absent.
pub fn ensure(data_dir: &Path) -> anyhow::Result<String> {
    let file = path(data_dir);
    match read(&file) {
        Ok(t) => return Ok(t),
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
    {
        Ok(mut f) => {
            f.write_all(format!("{token}\n").as_bytes())
                .with_context(|| format!("write {}", file.display()))?;
            f.sync_all().ok();
            Ok(token)
        }
        // Lost a creation race: use the winner's token.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read(&file),
        Err(e) => Err(e).with_context(|| format!("create {}", file.display())),
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == ErrorKind::NotFound)
}

fn read(file: &Path) -> anyhow::Result<String> {
    let s = std::fs::read_to_string(file).map_err(anyhow::Error::from)?;
    let t = s.trim();
    if t.len() < 32 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "{}: malformed token (delete it to regenerate)",
            file.display()
        );
    }
    Ok(t.to_string())
}

/// Permission bits of the token file, if it exists.
pub fn mode(data_dir: &Path) -> std::io::Result<Option<u32>> {
    match std::fs::metadata(path(data_dir)) {
        Ok(m) => Ok(Some(m.permissions().mode() & 0o777)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Constant-time byte comparison (length is not secret: tokens are fixed-size).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_then_reuses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested");
        let a = ensure(&dir).unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(mode(&dir).unwrap(), Some(0o600));
        assert_eq!(ensure(&dir).unwrap(), a);
        let other = tempfile::tempdir().unwrap();
        assert_ne!(ensure(other.path()).unwrap(), a);
    }

    #[test]
    fn malformed_token_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(path(tmp.path()), "short\n").unwrap();
        assert!(ensure(tmp.path()).is_err());
    }

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
