//! The broker's users: set, removed, listed, and logins checked.

use super::*;

impl Broker {
    /// Adds or changes a login.
    pub fn set_user(&self, name: &str, password: &str) -> io::Result<()> {
        if name.is_empty() || password.len() < 8 {
            return Err(idn_common::err::invalid_input(
                "a user name, and a password of 8 characters or more",
            ));
        }
        let h = idn_open_protocol::web::hash_password(password)?;
        let w = self.0.db.begin_write().map_err(db_err)?;
        w.open_table(USERS)
            .map_err(db_err)?
            .insert(name, h.as_str())
            .map_err(db_err)?;
        w.commit().map_err(db_err)?;
        self.log_out(name);
        Ok(())
    }

    /// Login `name`'s live connections closed: removed, or its password changed (one
    /// already connected went on publishing commands with the login gone; bug run 4).
    fn log_out(&self, name: &str) {
        let st = self.0.state.lock_ok();
        for (id, l) in &st.live {
            if st
                .sessions
                .get(id)
                .is_some_and(|s| s.user.as_deref() == Some(name))
            {
                let _ = l.tx.send(Out::LoginGone);
            }
        }
    }

    pub fn remove_user(&self, name: &str) -> io::Result<bool> {
        let w = self.0.db.begin_write().map_err(db_err)?;
        let gone = w
            .open_table(USERS)
            .map_err(db_err)?
            .remove(name)
            .map_err(db_err)?
            .is_some();
        w.commit().map_err(db_err)?;
        self.log_out(name);
        Ok(gone)
    }

    pub fn users(&self) -> io::Result<Vec<String>> {
        let r = self.0.db.begin_read().map_err(db_err)?;
        let t = r.open_table(USERS).map_err(db_err)?;
        Ok(t.iter()
            .map_err(db_err)?
            .filter_map(|e| e.ok().map(|(k, _)| k.value().to_owned()))
            .collect())
    }

    pub(super) fn check_login(&self, name: &str, password: &str) -> bool {
        let hash = self
            .0
            .db
            .begin_read()
            .ok()
            .and_then(|r| r.open_table(USERS).ok())
            .and_then(|t| t.get(name).ok().flatten().map(|v| v.value().to_owned()));
        // An unknown name is checked against a stand-in, so it takes as long as a known
        // one: the time doesn't tell which names exist.
        static STAND_IN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        match hash {
            Some(h) => idn_open_protocol::web::verify_password(&h, password),
            None => {
                let h = STAND_IN.get_or_init(|| {
                    idn_open_protocol::web::hash_password("stand-in").unwrap_or_default()
                });
                let _ = idn_open_protocol::web::verify_password(h, password);
                false
            }
        }
    }

    /// A login's check: a turn among the password checks first (at most four at once,
    /// each on a blocking thread), then the try counted against the address and the name
    /// ([`Logins`]: 5 wrong an address, 20 a name, in a minute), then the check. Counted in
    /// the turn, so a machine whose programs all log in at once isn't refused the right
    /// password for tries of its own still waiting (bug round E, E8-5), and CONNECTs sent
    /// at once can't all pass the count before any has failed (bug run 2026-11-25).
    pub(super) async fn login(&self, name: &str, password: &str, remote: &str) -> bool {
        // An address that isn't one (not over TCP) is counted with every other such.
        let from = remote
            .parse::<std::net::SocketAddr>()
            .map(|a| a.ip())
            .or_else(|_| remote.parse())
            .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED.into());
        let (me, name, password) = (self.clone(), name.to_owned(), password.to_owned());
        self.0
            .checks
            .run(move || {
                let logins = &me.0.logins;
                if !logins.attempt_as(from, &name) {
                    return false;
                }
                let ok = me.check_login(&name, &password);
                if ok {
                    logins.succeeded_as(from, &name);
                }
                ok
            })
            .await
            .unwrap_or(false)
    }
}
