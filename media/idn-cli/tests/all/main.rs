//! The crate's integration tests in one program: each file under
//! `tests/` was its own test program, linked again in full every build; on this shared
//! host the links and their output stalled the disk. Each module is a former file;
//! nextest still runs every test in a process of its own.

#[path = "../common/mod.rs"]
mod common;

mod api_output;
mod dispatch;
mod ds_loopback;
mod hx_loopback;
mod omt_bridge;
mod recv_loopback;
mod send_recv;
mod shutdown;
mod site_monitoring;
