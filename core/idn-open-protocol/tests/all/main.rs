//! The crate's integration tests in one program: each file under
//! `tests/` was its own test program, linked again in full every build; on this shared
//! host the links and their output stalled the disk. Each module is a former file;
//! nextest still runs every test in a process of its own.

#[path = "../common/mod.rs"]
mod common;

mod channel;
mod channels;
mod codec;
mod connect_failure;
mod discovery_loopback;
mod extensions;
mod fuzz_parsers;
mod gpu;
mod half_size;
mod join_code;
mod lan_tls;
mod link;
mod local;
mod machine;
mod mcast;
mod options;
mod parallel_decode;
mod picture_every;
mod ptp;
mod quic_loopback;
mod regions;
mod remote;
mod router;
mod samples_wanted;
mod secure;
mod shq_parallel;
mod shq_vectors;
mod silent_source;
mod site_here;
mod sitelink;
mod size_at_start;
mod sync;
mod tally;
mod trust;
