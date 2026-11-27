//! IDN Site Manager on its own (spec/extensions/idn-node.md §N.11.1, §N.12): the site
//! authority and its web panel, without the rest of IDN Media, for a server, a VPS or a
//! container (installer/docker/site-manager, the idn-site-manager Linux package). The
//! same program as `idn discovery-server`: its options are those.
//!
//!   idn-site-manager --trust-dir /var/lib/idn-site-manager [--panel-port 29202] …

use std::process::ExitCode;

fn main() -> ExitCode {
    // Freed memory given back to the system, not held by glibc (idn_common::memory).
    let _ = idn_common::memory::tune();
    idn_open_protocol::cli::set_tool("idn-site-manager", 1);
    match idn_site_manager::discovery_server::main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}
