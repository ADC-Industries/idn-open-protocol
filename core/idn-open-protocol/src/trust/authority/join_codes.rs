//! The site's join mode and its join codes (§E.10.10): issued by the Site Manager,
//! single use, 10 minutes, 5 tries, kept across restarts.

use super::*;
use crate::trust::join_code::{Burned, IssuedCode, JOIN_MODE_KEY, JoinCodeInfo, JoinMode};

impl Authority {
    /// How machines join this site: the site option `join.mode` (§E.20.3); a site without
    /// it is in [`JoinMode::Approval`].
    pub fn join_mode(&self) -> JoinMode {
        self.0
            .state
            .lock_ok()
            .options
            .for_device("")
            .get(JOIN_MODE_KEY)
            .and_then(JoinMode::parse)
            .unwrap_or_default()
    }

    /// Sets how machines join (the site option `join.mode`, pushed like any other).
    /// Returns the options' new version.
    pub fn set_join_mode(&self, mode: JoinMode) -> io::Result<u64> {
        self.set_option(
            &crate::trust::options::OptionScope::Site,
            JOIN_MODE_KEY,
            mode.as_str(),
        )
    }

    /// A new join code from `issuer` (a Site Manager user): six digits, good for one
    /// machine within 10 minutes and 5 wrong tries. Kept on disk at once. Past four codes
    /// waiting, the oldest is burned.
    pub fn issue_join_code(&self, issuer: &str) -> io::Result<IssuedCode> {
        let now = idn_common::epoch::secs();
        let (issued, burned) = {
            let mut st = self.0.state.lock_ok();
            let r = st.codes.issue(issuer, now);
            st.codes.save(&self.0.dir)?;
            r
        };
        self.burned(burned.into_iter());
        Ok(issued)
    }

    /// The join codes waiting, oldest first: their expiry and tries, not the codes.
    pub fn join_codes(&self) -> Vec<JoinCodeInfo> {
        let now = idn_common::epoch::secs();
        self.0.state.lock_ok().codes.info(now)
    }

    /// Takes the join code `id` back before it is used.
    pub fn cancel_join_code(&self, id: u64) -> io::Result<()> {
        let mut st = self.0.state.lock_ok();
        if !st.codes.cancel(id) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no join code {id}"),
            ));
        }
        st.codes.save(&self.0.dir)
    }

    pub(super) fn burned(&self, b: impl Iterator<Item = Burned>) {
        for b in b {
            self.event(match b {
                Burned::Tries(id) => AuthorityEvent::CodeBurned {
                    id,
                    replaced: false,
                },
                Burned::Replaced(id) => AuthorityEvent::CodeBurned { id, replaced: true },
            });
        }
    }

    /// One try of each waiting code taken for an exchange starting now (saved first, so
    /// that stopping half-way or a restart never gives a try back): (id, code).
    pub(super) fn take_code_tries(&self) -> io::Result<Vec<(u64, String)>> {
        let now = idn_common::epoch::secs();
        let mut st = self.0.state.lock_ok();
        st.codes.prune(now);
        let t = st.codes.take_tries(now);
        if !t.is_empty() {
            st.codes.save(&self.0.dir)?;
        }
        Ok(t)
    }

    /// The exchange proved the code `used` (single use: gone); the others it ran against
    /// get their try back.
    pub(super) fn code_matched(&self, used: u64, others: &[u64]) -> io::Result<()> {
        let mut st = self.0.state.lock_ok();
        st.codes.matched(used, others);
        st.codes.save(&self.0.dir)
    }

    /// The exchange over `ids` failed (a wrong code, or the device stopped): the tries
    /// stay taken, `addr` gets a failure. Returns the most tries left, and the codes
    /// burned now (for [`burned`](Self::burned), after the wrong try is told).
    pub(super) fn code_failed(&self, ids: &[u64], addr: Option<IpAddr>) -> (u32, Vec<Burned>) {
        let now = idn_common::epoch::secs();
        let (left, burned) = {
            let mut st = self.0.state.lock_ok();
            st.codes.note_fail(addr, now);
            let r = st.codes.failed(ids);
            if let Err(e) = st.codes.save(&self.0.dir) {
                crate::log::log(&format!("trust: keeping the join codes: {e}"));
            }
            r
        };
        (left, burned)
    }

    /// Seconds until `addr` may try a code again, if it is refused now (10 failures in
    /// 10 minutes).
    pub(super) fn code_limited(&self, addr: Option<IpAddr>) -> Option<u64> {
        let now = idn_common::epoch::secs();
        self.0.state.lock_ok().codes.limited(addr, now)
    }
}
