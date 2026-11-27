//! IDN in audio editors (N8, idn-routing.md §A.13): the plugins **IDN Send** and **IDN
//! Return**, in CLAP and VST3 from one library. Send makes a track an IDN source
//! (`HOST (REAPER Send 3)`); Return is a routable receiver (`REAPER Return 3`) on the
//! machine's IDN Node, so the Site Manager's matrix routes any IDN sound into a track.
//!
//! The library is installed as `IDN.clap` and inside `IDN.vst3` (scripts/package.sh); the
//! same file answers both formats.

pub mod clap;
pub mod engine;
mod vst3;

clack_plugin::clack_export_entry!(clap::IdnEntry);
