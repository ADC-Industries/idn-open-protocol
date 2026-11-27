//! Linked computers (pair/1, §E.28) and tracked sessions, closed when a peer is revoked or unlinked.

use super::*;

impl Enrollment {
    /// `x_idn_ca` (§E.10.7), once enrolled.
    /// The machine's linked computers (§E.28), as the IDN Node gives them: empty while
    /// it is in a site.
    pub fn pairs(&self) -> Arc<Vec<super::super::pair::Linked>> {
        self.pairs.read_ok().clone()
    }

    /// A session using the link with `fp` (§E.28), counted while the lease is held and told
    /// to the IDN Node: a link for this time only ends with its last session.
    pub(crate) fn pair_lease(&self, fp: &str) -> Arc<PairLease> {
        self.pair_use(fp, true);
        Arc::new(PairLease {
            e: self.me.get().cloned().unwrap_or_default(),
            fp: fp.to_owned(),
        })
    }

    fn pair_use(&self, fp: &str, more: bool) {
        let n = {
            let mut u = self.pair_uses.lock_ok();
            let n = u.entry(fp.to_owned()).or_insert(0);
            *n = if more { *n + 1 } else { n.saturating_sub(1) };
            let n = *n;
            if n == 0 {
                u.remove(fp);
            }
            n
        };
        if let Some(node) = &self.node {
            node.send(
                Element::new("pair_use")
                    .with_attr("fp", fp)
                    .with_attr("n", &n.to_string()),
            );
        }
    }

    /// Sets the linked computers of a membership the IDN Node doesn't hold (the Node
    /// gives its programs its own, replacing these when it next speaks).
    #[doc(hidden)]
    pub fn set_pairs(&self, pairs: Vec<super::super::pair::Linked>) {
        *self.pairs.write_ok() = Arc::new(pairs);
        self.close_unlinked();
    }

    /// Records a live `pair/1` session with linked computer `fp`.
    pub(crate) fn track_pair(&self, fp: &str, net: &TcpStream) {
        if let Ok(sock) = net.try_clone() {
            let mut s = self.pair_sessions.lock_ok();
            s.retain(|(_, t)| session_alive(t));
            s.push((fp.to_owned(), sock));
        }
        // Unlinked meanwhile (between its handshake and now): closed at once.
        self.close_unlinked();
    }

    /// Ends the `pair/1` sessions of computers no longer linked.
    pub(super) fn close_unlinked(&self) {
        let linked: Vec<String> = self.pairs.read_ok().iter().map(|l| l.fp.clone()).collect();
        self.pair_sessions.lock_ok().retain(|(fp, t)| {
            if linked.contains(fp) {
                true
            } else {
                let _ = t.shutdown(std::net::Shutdown::Both);
                false
            }
        });
    }

    /// Records a live `secure/2` session with a peer, to close it if the peer is revoked.
    /// An error (and the connection shut) if the peer was revoked by now, or this device
    /// lost its own credentials meanwhile: such a session must not go on untracked,
    /// beyond the reach of later revocations (§E.10.6, R1-112). With `tell` (`net` is the
    /// session's plain-text control stream, as on the link), the peer is told why first
    /// (a revoked receiver only saw a broken pipe).
    pub(crate) fn track_session(
        &self,
        peer_cert: &[u8],
        net: &TcpStream,
        tell: bool,
    ) -> io::Result<()> {
        let refused = |why: &str| {
            if tell {
                crate::secure::tell_refused(net, why);
            }
        };
        let tracked = (|| {
            let Some(c) = self.creds() else {
                refused("this device has no site credentials any more");
                return Err(err("no device credentials any more"));
            };
            let info = c.anchor.verify(peer_cert).inspect_err(|_| {
                refused(
                    "your site certificate is no longer accepted (revoked by the site authority)",
                )
            })?;
            let sock = net.try_clone()?;
            let mut s = self.sessions.lock_ok();
            // Forget sessions that have ended.
            s.retain(|(_, t)| session_alive(t));
            s.push((info.serial, sock));
            Ok::<_, io::Error>(())
        })();
        if tracked.is_err() {
            let _ = net.shutdown(std::net::Shutdown::Both);
        }
        tracked
    }
}

/// A session's hold on a link (§E.28): counted on the membership while it lives.
#[derive(Debug)]
pub struct PairLease {
    e: std::sync::Weak<Enrollment>,
    fp: String,
}

impl PairLease {
    /// The session using this link, closed if the link is removed.
    pub(crate) fn track(&self, net: &TcpStream) {
        if let Some(e) = self.e.upgrade() {
            e.track_pair(&self.fp, net);
        }
    }
}

impl Drop for PairLease {
    fn drop(&mut self) {
        if let Some(e) = self.e.upgrade() {
            e.pair_use(&self.fp, false);
        }
    }
}
