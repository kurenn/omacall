//! Contacts: names to endpoint ids.
//!
//! Addresses are deliberately never stored. v1 kept `name host-or-ip` and broke
//! every time DHCP moved a machine -- the Mac used for testing moved across
//! three LAN addresses in a single afternoon. The id is stable forever; where a
//! peer currently *is* gets resolved at dial time.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Contacts {
    #[serde(default)]
    pub peers: BTreeMap<String, Contact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub id: EndpointId,
    /// Ring this peer even though the call did not come from the local network.
    #[serde(default)]
    pub allow_internet_ring: bool,
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

    pub fn add(&mut self, name: impl Into<String>, id: EndpointId) {
        self.peers.insert(
            name.into(),
            Contact { id, allow_internet_ring: false },
        );
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
        c.add("carlos", id);
        c.save(&path).unwrap();

        let back = Contacts::load(&path).unwrap();
        assert_eq!(back.lookup("carlos").unwrap().id, id);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let mut c = Contacts::default();
        let id = some_id();
        c.add("Carlos", id);
        assert_eq!(c.lookup("carlos").unwrap().id, id);
        assert_eq!(c.lookup("CARLOS").unwrap().id, id);
        assert!(c.lookup("someone-else").is_none());
    }

    #[test]
    fn ids_map_back_to_names_for_incoming_calls() {
        let mut c = Contacts::default();
        let id = some_id();
        c.add("carlos", id);
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
    fn stored_form_carries_no_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.toml");
        let mut c = Contacts::default();
        c.add("carlos", some_id());
        c.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("192.168"), "addresses must never be persisted");
        assert!(text.contains("[peers.carlos]"));
    }
}
