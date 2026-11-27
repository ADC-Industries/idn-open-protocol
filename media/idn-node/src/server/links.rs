//! Linked computers without a site (§N.32): asking, answering, finding and removing
//! links, and the port other computers link at.

use super::*;

impl NodeCore {
    /// What tells the programs when the links change: the identity again.
    pub(super) fn pairs_changed(&self) -> crate::pairs::Changed {
        let me = self.me.get().cloned().unwrap_or_default();
        Arc::new(move || {
            if let Some(c) = me.upgrade() {
                for line in c.pairs.news() {
                    c.log(&line);
                }
                c.log("linked computers changed");
                let _ = c.events.send(c.identity().to_xml());
            }
        })
    }

    /// The links, the open window and the last outcome, for the panel and `idn-node link`.
    pub fn pairs_status(&self) -> Element {
        let site = self.in_site();
        self.pairs
            .element(!site, true)
            .with_attr("in_site", if site { "true" } else { "false" })
    }

    /// Asks the computer at `addr` to link (IDN Remote, the panel; §N.32): its name and the
    /// code both show; its person's answer comes as the outgoing request's state.
    pub fn pair_ask(&self, addr: std::net::SocketAddr) -> Result<(String, String), String> {
        if self.in_site() {
            return Err("this computer is in a site: it uses the site, not links".into());
        }
        self.pairs
            .ask(
                addr,
                &self.settings().name,
                &self.device_key.spki(),
                self.pairs_changed(),
            )
            .map_err(|e| e.to_string())
    }

    /// This machine's person's word on the request it made: the codes match (kept once
    /// accepted there), or cancel (§N.32).
    pub fn pair_confirm(&self, ok: bool, code: Option<&str>) -> Result<(), String> {
        let r = self.pairs.confirm(ok, code);
        (self.pairs_changed())();
        r
    }

    /// This machine's person's answer to the request waiting (§N.32).
    pub fn pair_answer(
        &self,
        accept: bool,
        remember: bool,
        code: Option<&str>,
    ) -> Result<(), String> {
        self.pairs.answer(accept, remember, code)
    }

    /// The other computers that take links on the network (not this one).
    pub fn pair_find(&self) -> Vec<idn_open_protocol::discovery::service::Found> {
        let (name, port) = (self.settings().name, self.pairs.port());
        crate::pairs::find()
            .into_iter()
            .filter(|f| !(f.instance == name && Some(f.port) == port))
            .collect()
    }

    /// Starts the port other computers link at (from the Node's start; §N.32).
    pub(super) fn listen_for_links(&self) {
        let me = self.me.get().cloned().unwrap_or_default();
        let who: crate::pairs::Me = Arc::new(move || {
            let c = me.upgrade()?;
            if c.in_site() {
                return None;
            }
            let s = c.settings();
            Some((s.name, c.device_key.spki(), s.link_requests))
        });
        match self.pairs.listen(who, self.pairs_changed()) {
            Ok(port) => self.log(&format!("links: port {port}")),
            Err(e) => self.log(&format!("links: no port ({e})")),
        }
    }

    /// Forgets a link (fingerprint, whole or its first 16 digits).
    pub fn pair_remove(&self, fp: &str) -> Result<(), String> {
        if !self.pairs.remove(fp) {
            return Err("no such linked computer".into());
        }
        (self.pairs_changed())();
        Ok(())
    }
}
