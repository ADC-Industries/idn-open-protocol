//! One program's connection to the Node (§N.4): its hello, its requests, until it goes.

use super::*;

impl NodeCore {
    /// Serves one program on `stream` until it disconnects (§N.4).
    pub async fn serve<S: AsyncRead + AsyncWrite + Send + 'static>(
        self: Arc<Self>,
        stream: S,
        peer: Peer,
    ) -> io::Result<()> {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let hello = tokio::time::timeout(Duration::from_secs(5), wire::read(&mut rd))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no node_hello"))??
            .ok_or_else(|| io::Error::other("closed before node_hello"))?;
        if hello.name != "node_hello" {
            return Err(io::Error::other(format!(
                "expected node_hello, got {}",
                hello.name
            )));
        }
        if hello.attr("v").and_then(|v| v.parse::<u32>().ok()) != Some(MAJOR) {
            let refused = Element::new("node_refused")
                .with_attr("v", &MAJOR.to_string())
                .with_attr(
                    "reason",
                    &format!(
                        "this Node speaks version {MAJOR}, the program {}",
                        hello.attr("v").unwrap_or("?")
                    ),
                );
            wire::write(&mut wr, &refused).await?;
            return Ok(());
        }
        // (No control characters, at most 128: the name goes into the program rules,
        // one a line, and the panel; bug run 2026-11-24.)
        let app: String = hello
            .attr("app")
            .unwrap_or("unnamed program")
            .chars()
            .take(128)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned();
        let app = if app.is_empty() {
            "unnamed program".to_owned()
        } else {
            app
        };
        let pid = peer
            .pid
            .or_else(|| hello.attr("pid").and_then(|p| p.parse().ok()))
            .unwrap_or(0);
        let exe = peer.exe.clone().unwrap_or_default();
        // Its account: its uid (Unix), or its SIDs asked of the pipe now that it has
        // written (Windows).
        #[cfg(windows)]
        let owner = peer
            .pipe
            .and_then(crate::ipc::pipe_client_sids)
            .map(idn_open_protocol::diag::Owner::Sids);
        #[cfg(not(windows))]
        let owner = peer.uid.map(idn_open_protocol::diag::Owner::Uid);
        let approval = self.approval(&exe);
        if approval == Approval::Blocked {
            self.log(&format!(
                "program refused (blocked in the panel): {app} ({exe})"
            ));
            let refused = Element::new("node_refused")
                .with_attr("v", &MAJOR.to_string())
                .with_attr(
                    "reason",
                    "this program is blocked in the IDN Node's panel (Programs)",
                );
            wire::write(&mut wr, &refused).await?;
            return Ok(());
        }
        let mut welcome = Element::new("node_welcome")
            .with_attr("v", &MAJOR.to_string())
            .with_attr("node", env!("CARGO_PKG_VERSION"))
            .with_attr("fp", &self.fingerprint())
            .with_attr("name", &self.settings().name);
        if approval == Approval::Pending {
            welcome = welcome.with_attr("approval", "pending");
            self.log(&format!(
                "program waiting for approval in the panel: {app} ({})",
                if exe.is_empty() { "path unknown" } else { &exe }
            ));
        }
        wire::write(&mut wr, &welcome).await?;
        let cid = self.next.fetch_add(1, Ordering::Relaxed);
        // One writer: replies, events and messages for this program alone.
        let (raw_tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let tx = super::Outbox::new(raw_tx);
        self.clients.lock_ok().insert(
            cid,
            Client {
                app: app.clone(),
                pid,
                user: peer.user.clone(),
                admin: peer.admin,
                uid: peer.uid,
                owner,
                exe,
                since: SystemTime::now(),
                regs: BTreeMap::new(),
                out: Some(tx.clone()),
                services: None,
            },
        );
        self.log(&format!(
            "program connected: {app} (pid {pid}, {})",
            peer.user
        ));
        let mut events = self.events.subscribe();
        let etx = tx.clone();
        let forward = tokio::spawn(async move {
            while let Ok(m) = events.recv().await {
                if !etx.send(m) {
                    break;
                }
            }
        });
        let wtx = tx.clone();
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let n = m.len();
                let ok = wire::write_str(&mut wr, &m).await.is_ok();
                wtx.sent(n);
                if !ok {
                    break;
                }
            }
        });
        // At most this many of one program's requests at once: one program pipelining
        // thousands filled the blocking pool, and everyone else's waited. Reading waits
        // for a place.
        let in_flight = Arc::new(tokio::sync::Semaphore::new(8));
        let result = loop {
            // (A program whose replies piled up unread, or whose writer failed: ended.)
            if tx.over() || writer.is_finished() {
                break Err(std::io::Error::other(
                    "its messages weren't read: connection ended",
                ));
            }
            let m = match wire::read(&mut rd).await {
                Ok(Some(m)) => m,
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            };
            let Ok(place) = in_flight.clone().acquire_owned().await else {
                break Ok(());
            };
            let core = self.clone();
            let tx = tx.clone();
            // Requests may wait (an admin request, a passphrase derivation): off the
            // runtime's threads, answered in any order (each reply names its id).
            tokio::task::spawn_blocking(move || {
                let _place = place;
                let reply = core.handle(cid, &m);
                tx.send(reply.to_xml());
            });
        };
        forward.abort();
        // The program's entry holds a sender of the writer's queue (`Client::out`): it
        // goes first, or the writer would wait for more forever and the entry stay.
        self.pairs.client_gone(cid);
        if let Some(c) = self.clients.lock_ok().remove(&cid) {
            self.log(&format!(
                "program disconnected: {} ({} registration(s) dropped)",
                c.app,
                c.regs.len()
            ));
        }
        drop(tx);
        let _ = writer.await;
        result
    }
}
