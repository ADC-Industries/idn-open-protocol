//! Routers: sources with no media of their own whose receivers follow the source they are
//! switched to (a routing source, §1.8).

use super::*;

impl Router {
    /// Starts the router `name` (`HOST (name)` on the network), routing nowhere yet.
    pub fn new(publisher: &Publisher, name: &str) -> io::Result<Router> {
        Self::on_port(publisher, name, 0)
    }

    /// [`Router::new`] on TCP port `port` (0: any free one), for a firewall's rule.
    pub fn on_port(publisher: &Publisher, name: &str, port: u16) -> io::Result<Router> {
        let route = Arc::new(crate::sender::Route::default());
        let output = Output::start(
            publisher,
            OutputConfig {
                port,
                ..OutputConfig::new(name)
            },
            Some(route.clone()),
        )?;
        let wanted: Arc<Mutex<Option<String>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let browser = crate::discovery::shared(crate::discovery::BrowserConfig {
            interface: None,
            discovery_server: publisher.0.server.map(|a| a.to_string()),
        })?;
        let me = output.name().to_owned();
        // The wanted source's address, looked up by name (again every half second: a
        // source that moves or comes back later is followed).
        let finder = {
            let (route, wanted, stop) = (route.clone(), wanted.clone(), stop.clone());
            std::thread::Builder::new()
                .name("idn-router".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let want = wanted.lock_ok().clone();
                        if let Some(w) = want.filter(|w| !w.eq_ignore_ascii_case(&me)) {
                            let found = match w.parse::<SocketAddr>() {
                                Ok(a) => Some((w.clone(), a)),
                                Err(_) => browser
                                    .sources()
                                    .into_iter()
                                    .find(|s| s.name.eq_ignore_ascii_case(&w))
                                    .and_then(|s| Some((s.name.clone(), s.socket_addr()?))),
                            };
                            // Not found (yet): receivers stay where they are.
                            if found.is_some() {
                                route.set(found);
                            }
                        }
                        // (Woken at once when the router is dropped.)
                        std::thread::park_timeout(std::time::Duration::from_millis(500));
                    }
                })?
        };
        Ok(Router {
            output,
            route,
            wanted,
            stop,
            finder: Some(finder),
        })
    }

    /// Switches the router to `source` (`HOST (name)`, or `address:port`): its receivers
    /// move there as soon as it is found on the network. `None` leaves them where they
    /// are.
    pub fn route(&self, source: Option<&str>) {
        *self.wanted.lock_ok() = source
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
    }

    /// The source asked for with [`Router::route`].
    pub fn wanted(&self) -> Option<String> {
        self.wanted.lock_ok().clone()
    }

    /// The source receivers are being sent to, once found: its full name.
    pub fn target(&self) -> Option<String> {
        self.route.get().map(|(name, _)| name)
    }

    /// The router's full name on the network, `HOST (name)`.
    pub fn name(&self) -> &str {
        self.output.name()
    }

    /// Receivers connected to the router now.
    pub fn receivers(&self) -> usize {
        self.output.receivers()
    }

    /// The router's TCP port.
    pub fn port(&self) -> u16 {
        self.output.port()
    }
}
