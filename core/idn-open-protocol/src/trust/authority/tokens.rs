//! Access tokens signed by the site (idn-node.md §N.31.5): the claims made by the Site
//! Manager, signed here with the site CA's key.

use super::*;
use crate::trust::token::{self, Claims, Scope};

impl Authority {
    /// The site's id in tokens and certificates (`x_idn_ca`: the first 16 hex digits of
    /// the CA's fingerprint).
    pub fn site_id(&self) -> String {
        self.0.anchor.ca_id().to_owned()
    }

    /// A token for `user` (`name` for people) to use `engine` on `machine` (a
    /// fingerprint, or `*` for any of the site's machines) with `scope`, living `ttl`
    /// seconds (at most 12 hours): signed with the site CA's key.
    pub fn issue_token(
        &self,
        user: &str,
        name: &str,
        engine: &str,
        machine: &str,
        scope: Scope,
        ttl: u64,
    ) -> io::Result<(String, Claims)> {
        let c = Claims::new(
            &self.site_id(),
            user,
            name,
            engine,
            machine,
            scope,
            idn_common::epoch::secs(),
            ttl,
        )?;
        Ok((token::sign(&c, self.0.ca.key())?, c))
    }

    /// A named API key `id` (`name` for people) for `engine` on `machine` with `scope`:
    /// a token living [`token::MAX_KEY_LIFE`], its subject `key:<id>` (withdrawn by that
    /// subject's not-before).
    pub fn issue_key(
        &self,
        id: &str,
        name: &str,
        engine: &str,
        machine: &str,
        scope: Scope,
    ) -> io::Result<(String, Claims)> {
        self.issue_key_lasting(id, name, engine, machine, scope, token::MAX_KEY_LIFE)
    }

    /// [`issue_key`](Self::issue_key) living `life` seconds (a day to
    /// [`token::MAX_KEY_LIFE`]): a key re-pairing replaces, whose revocation the site then
    /// forgets once it would have ended anyway.
    pub fn issue_key_lasting(
        &self,
        id: &str,
        name: &str,
        engine: &str,
        machine: &str,
        scope: Scope,
        life: u64,
    ) -> io::Result<(String, Claims)> {
        let mut c = Claims::new_key(
            &self.site_id(),
            id,
            name,
            engine,
            machine,
            scope,
            idn_common::epoch::secs(),
        )?;
        c.exp = c.iat + life.clamp(86_400, token::MAX_KEY_LIFE);
        Ok((token::sign(&c, self.0.ca.key())?, c))
    }

    /// The site CA's Ed25519 public key (what engines check tokens with).
    pub fn ca_public(&self) -> [u8; 32] {
        self.0.ca.key().public
    }
}
