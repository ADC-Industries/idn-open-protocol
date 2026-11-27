//! Work in bands on the protocol library's worker pool, in the shape of
//! `std::thread::scope`: jobs are gathered, then run on the pool's threads and this one,
//! returning when all are done. (Threads started for each band of each plane of each
//! operation, 100–150 a CPU frame with a transition and overlays, and the core count read
//! from the cgroup files each time.)

/// The jobs of one [`scope`].
pub struct Bands<'a> {
    jobs: Vec<Box<dyn FnOnce() + Send + 'a>>,
}

impl<'a> Bands<'a> {
    /// A job to run (as `std::thread::Scope::spawn`).
    pub fn spawn(&mut self, f: impl FnOnce() + Send + 'a) {
        self.jobs.push(Box::new(f));
    }
}

/// Gathers the jobs `build` gives, then runs them all on the pool.
pub fn scope<'a>(build: impl FnOnce(&mut Bands<'a>)) {
    let mut b = Bands { jobs: Vec::new() };
    build(&mut b);
    idn_open_protocol::pool::for_each(b.jobs, |_, f| f());
}

/// How many bands to split work into: the pool's threads, at most 8.
pub fn threads() -> usize {
    idn_open_protocol::pool::threads().clamp(1, 8)
}
