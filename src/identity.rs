//! The machine's identity: one ed25519 keypair, and the ticket that lets
//! someone else reach it.
//!
//! The key *is* the identity. There is no account, no registration and no
//! rotation story -- if it leaks, the only recourse is a new key and re-sending
//! tickets to every contact. That is an accepted position for a
//! friends-and-family tool, but it is why a corrupt key file must never be
//! silently replaced: doing so would destroy the user's identity with every
//! contact they have, at the moment they can least afford it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use iroh::{EndpointAddr, SecretKey};

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OMACALL_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".config/omacall")
}

pub fn key_path() -> PathBuf {
    config_dir().join("key")
}

/// Load the keypair, creating one on first run.
///
/// A file that exists but does not parse is a hard error. Falling through to
/// "create" would mint a new identity and silently orphan every contact.
pub fn load_or_create(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let raw = std::fs::read(path).with_context(|| format!("reading {path:?}"))?;
        let bytes: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "{path:?} is {} bytes, not 32. Refusing to replace it: if this key is lost, \
                 every contact has to be re-added. Move it aside deliberately to start over.",
                raw.len()
            )
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    let key = SecretKey::generate();
    write_key(path, &key)?;
    Ok(key)
}

fn write_key(path: &Path, key: &SecretKey) -> Result<()> {
    std::fs::write(path, key.to_bytes()).with_context(|| format!("writing {path:?}"))?;
    restrict(path)
}

/// A private key readable by anyone else on the machine is not private.
fn restrict(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {path:?} to 0600"))?;
    }
    Ok(())
}

/// Warn if an existing key is group- or world-readable.
pub fn check_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!("{path:?} is mode {mode:o}; it must be 0600");
        }
    }
    Ok(())
}

/// A ticket is just the endpoint's address: its id plus the addresses it can
/// currently be reached at.
///
/// Dialing a bare id needs pkarr/DNS discovery to have published *and*
/// propagated, which fails outright on a cold start -- measured during the
/// spike, which failed with "All address lookup services failed". Carrying the
/// addresses makes first contact work immediately.
pub fn encode_ticket(addr: &EndpointAddr) -> Result<String> {
    Ok(serde_json::to_string(addr)?)
}

pub fn decode_ticket(s: &str) -> Result<EndpointAddr> {
    let s = s.trim();
    serde_json::from_str(s).context("that does not look like an omacall ticket")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_key_on_first_run_and_reuses_it_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");

        let first = load_or_create(&path).unwrap();
        assert!(path.exists());
        let second = load_or_create(&path).unwrap();
        assert_eq!(
            first.public(),
            second.public(),
            "a second run must not mint a new identity"
        );
    }

    #[test]
    fn new_keys_are_written_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        load_or_create(&path).unwrap();
        check_permissions(&path).expect("a fresh key must not be group or world readable");
    }

    #[test]
    fn a_corrupt_key_is_an_error_not_a_fresh_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, b"not a key").unwrap();

        let err = load_or_create(&path).unwrap_err().to_string();
        assert!(err.contains("Refusing to replace"), "unhelpful error: {err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"not a key",
            "the existing file must be left untouched"
        );
    }

    #[test]
    fn loose_permissions_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        load_or_create(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(check_permissions(&path).is_err());
        }
    }

    #[test]
    fn ticket_round_trips() {
        let key = SecretKey::generate();
        let addr = EndpointAddr::from(key.public());
        let ticket = encode_ticket(&addr).unwrap();
        assert_eq!(decode_ticket(&ticket).unwrap().id, addr.id);
    }

    #[test]
    fn a_ticket_survives_surrounding_whitespace() {
        let key = SecretKey::generate();
        let addr = EndpointAddr::from(key.public());
        let ticket = format!("  {}\n", encode_ticket(&addr).unwrap());
        assert_eq!(decode_ticket(&ticket).unwrap().id, addr.id);
    }

    #[test]
    fn nonsense_tickets_are_rejected_with_a_readable_message() {
        let err = decode_ticket("hello").unwrap_err().to_string();
        assert!(err.contains("omacall ticket"), "unhelpful error: {err}");
    }
}
