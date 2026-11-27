//! Trying a site before joining it: the site the Node follows for a while, and leaving it again.

use super::*;

impl NodeCore {
    /// The site being asked to join, found on the network and not kept yet, if any.
    pub fn trial_site(&self) -> Option<TrialSite> {
        self.trial_site.lock_ok().clone()
    }

    /// Asks the site found at `server` to join, without keeping its address: kept once it
    /// approves and this machine joins ([`NodeCore::follow_trial_site`]).
    pub fn try_site(&self, server: String, name: String) {
        *self.trial_site.lock_ok() = Some(TrialSite {
            server,
            name,
            failing_since: None,
        });
        self.rebuild_member(None);
    }

    /// Each round of the site loop: a trial site that approved this machine (it joined) is
    /// kept as the server; one that denied it, or hasn't answered for 2 minutes, is
    /// forgotten, and the search goes on.
    pub fn follow_trial_site(&self) {
        let Some(t) = self.trial_site() else { return };
        if self.settings().server.is_some() {
            *self.trial_site.lock_ok() = None;
            return;
        }
        let state = self.enrollment().map(|e| e.state());
        match state {
            Some(TrustState::Enrolled { .. }) => {
                *self.trial_site.lock_ok() = None;
                self.log(&format!(
                    "site: {} approved this machine; its address {} kept",
                    t.name, t.server
                ));
                // Kept as it is: the membership already runs on it (restarting it would
                // drop the connection the site just approved).
                let _one_at_a_time = self.changing.lock_ok();
                let mut held = self.settings.lock_ok();
                if held.server.is_none() {
                    let mut s = held.clone();
                    s.server = Some(t.server);
                    match s.save(&self.dir.join("node.xml")) {
                        Ok(()) => *held = s,
                        Err(e) => self.log(&format!("site: keeping its address: {e}")),
                    }
                }
            }
            Some(TrustState::Denied) | Some(TrustState::Revoked) => {
                self.drop_trial_site("it refused this machine")
            }
            // Waiting for its approval (or this machine's confirmation of its code): fine.
            Some(TrustState::Pending { .. })
            | Some(TrustState::ConfirmCode { .. })
            | Some(TrustState::WaitingForPairing)
            | Some(TrustState::CodeNeeded)
            | Some(TrustState::CodeAccepted)
            | Some(TrustState::WrongCode { .. })
            | Some(TrustState::CodeExpired) => {
                if let Some(x) = self.trial_site.lock_ok().as_mut() {
                    x.failing_since = None;
                }
            }
            _ => {
                let since = {
                    let mut g = self.trial_site.lock_ok();
                    let Some(x) = g.as_mut() else { return };
                    *x.failing_since.get_or_insert_with(Instant::now)
                };
                if since.elapsed() >= Duration::from_secs(120) {
                    self.drop_trial_site("it hasn't answered for 2 minutes");
                }
            }
        }
    }

    pub(super) fn drop_trial_site(&self, why: &str) {
        let Some(t) = self.trial_site.lock_ok().take() else {
            return;
        };
        self.log(&format!(
            "site: not joining {} at {}: {why}; looking for a site again",
            t.name, t.server
        ));
        self.rebuild_member(None);
    }
}
