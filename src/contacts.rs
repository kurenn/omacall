//! Contacts: names to endpoint ids.
//!
//! The id is the identity and is stable forever. Addresses are stored too, but
//! only as a **hint**, and that distinction is the whole point: v1 kept `name
//! host-or-ip` and treated the address *as* the identity, so it broke every
//! time DHCP moved a machine -- the Mac used for testing took three LAN
//! addresses in one afternoon.
//!
//! The hint exists because dialing a bare id needs pkarr/DNS discovery to have
//! published and propagated, which measured at roughly 45 seconds after a
//! daemon starts. Without a hint, the first call after boot fails with "could
//! not reach". With one, the dial goes straight out and discovery is the
//! fallback rather than the critical path. Stale hints cost nothing: the id
//! still resolves.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Contacts {
    #[serde(default)]
    pub peers: BTreeMap<String, Contact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub id: EndpointId,
    /// Last known addresses. A cache, never an identity: wrong entries are
    /// harmless because the id still resolves through discovery.
    #[serde(default)]
    pub addrs: BTreeSet<TransportAddr>,
    /// Ring this peer even though the call did not come from the local network.
    #[serde(default)]
    pub allow_internet_ring: bool,
}

impl Contact {
    /// What to dial: the id, plus any cached addresses to try immediately.
    pub fn addr(&self) -> EndpointAddr {
        let mut a = EndpointAddr::from(self.id);
        a.addrs = self.addrs.clone();
        a
    }
}

impl Contacts {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
        toml::from_str(&raw).with_context(|| format!("parsing {path:?}"))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {path:?}"))
    }

    pub fn lookup(&self, name: &str) -> Option<&Contact> {
        self.peers.get(name).or_else(|| {
            // Names come from humans, so match case-insensitively rather than
            // making someone remember how they capitalised it.
            self.peers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
    }

    pub fn name_of(&self, id: &EndpointId) -> Option<&str> {
        self.peers
            .iter()
            .find(|(_, c)| &c.id == id)
            .map(|(k, _)| k.as_str())
    }

    pub fn add(&mut self, name: impl Into<String>, addr: &EndpointAddr) {
        self.peers.insert(
            name.into(),
            Contact {
                id: addr.id,
                addrs: addr.addrs.clone(),
                allow_internet_ring: false,
            },
        );
    }

    /// Refresh the hint after a successful connection, so it tracks a machine
    /// that moves rather than going stale forever.
    pub fn refresh(&mut self, addr: &EndpointAddr) {
        if let Some((_, c)) = self.peers.iter_mut().find(|(_, c)| c.id == addr.id) {
            c.addrs = addr.addrs.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn some_id() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn missing_file_is_an_empty_book_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = Contacts::load(&dir.path().join("nope.toml")).unwrap();
        assert!(c.peers.is_empty());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.toml");
        let id = some_id();

        let mut c = Contacts::default();
        c.add("carlos", &EndpointAddr::from(id));
        c.save(&path).unwrap();

        let back = Contacts::load(&path).unwrap();
        assert_eq!(back.lookup("carlos").unwrap().id, id);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let mut c = Contacts::default();
        let id = some_id();
        c.add("Carlos", &EndpointAddr::from(id));
        assert_eq!(c.lookup("carlos").unwrap().id, id);
        assert_eq!(c.lookup("CARLOS").unwrap().id, id);
        assert!(c.lookup("someone-else").is_none());
    }

    #[test]
    fn ids_map_back_to_names_for_incoming_calls() {
        let mut c = Contacts::default();
        let id = some_id();
        c.add("carlos", &EndpointAddr::from(id));
        assert_eq!(c.name_of(&id), Some("carlos"));
        assert_eq!(c.name_of(&some_id()), None);
    }

    #[test]
    fn a_broken_file_is_reported_rather_than_silently_emptied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();
        assert!(
            Contacts::load(&path).is_err(),
            "silently returning an empty book would look like every contact vanished"
        );
    }

    #[test]
    fn the_id_survives_a_hint_going_stale() {
        // The hint is a cache, so a wrong address must not break the contact:
        // this is exactly where v1 fell over, because there the address *was*
        // the identity.
        let id = some_id();
        let mut c = Contacts::default();
        c.add("carlos", &EndpointAddr::from(id));
        let moved = EndpointAddr::from(id);
        c.refresh(&moved);
        assert_eq!(c.lookup("carlos").unwrap().id, id, "identity is the id, not the address");
    }
}
