//! The desk's round trip measured (§M.28.2): a click sent out of one output, its output
//! otherwise silent while it runs, and listened for on one input looped back to it (a
//! cable, a sound card's loopback, or a route over the network); the time between them.

use super::*;

/// How loud the click is, and how loud it must come back.
const CLICK: f32 = 0.9;
const HEARD: f32 = 0.1;
/// Given up after this long.
pub const GIVE_UP_S: f32 = 2.0;

/// [`GIVE_UP_S`], for the engine.
pub fn roundtrip_give_up_s() -> f32 {
    GIVE_UP_S
}

/// A round-trip test in progress, or its result.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum RoundTrip {
    #[default]
    Idle,
    /// Asked for: the output (from 0) and input (from 0); the click goes out once the
    /// input has been quiet a whole block (waited `blocks` so far).
    Asked {
        output: usize,
        input: usize,
        blocks: usize,
    },
    /// The click went out `blocks` blocks ago.
    Sent {
        output: usize,
        input: usize,
        blocks: usize,
    },
    /// Heard back after this many samples.
    Done {
        samples: usize,
        output: usize,
        input: usize,
    },
    /// Why not.
    Failed(String),
}

impl Desk {
    /// Starts a round-trip test from desk output `output` to desk input `input` (both
    /// from 1).
    pub fn start_round_trip(&mut self, output: usize, input: usize) -> Result<(), String> {
        let (inputs, outputs) = self.io();
        if !(1..=outputs).contains(&output) {
            return Err(format!("output: 1 to {outputs}"));
        }
        if !(1..=inputs).contains(&input) {
            return Err(format!("input: 1 to {inputs}"));
        }
        self.round_trip = RoundTrip::Asked {
            output: output - 1,
            input: input - 1,
            blocks: 0,
        };
        Ok(())
    }

    /// The test's state now.
    pub fn round_trip(&self) -> RoundTrip {
        self.round_trip.clone()
    }

    /// Before a block: listen for the click on the test's input.
    pub(super) fn round_trip_listen(&mut self, input: &[Vec<f32>]) {
        let give_up = (GIVE_UP_S * rate()) as usize / self.block.max(1);
        let got = |i: usize| input.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
        self.round_trip = match std::mem::take(&mut self.round_trip) {
            // (Its output is silent meanwhile: what it still hears is something else.)
            RoundTrip::Asked {
                input: i, blocks, ..
            } if blocks >= give_up => RoundTrip::Failed(format!(
                "input {} wasn't quiet for {GIVE_UP_S} s with the output silent: not a loop of it alone",
                i + 1
            )),
            RoundTrip::Asked {
                output,
                input: i,
                blocks,
            } => RoundTrip::Asked {
                output,
                input: i,
                blocks: blocks + 1,
            },
            RoundTrip::Sent {
                output,
                input: i,
                blocks,
            } => match got(i).iter().position(|v| v.abs() >= HEARD) {
                Some(k) => RoundTrip::Done {
                    samples: blocks * self.block + k,
                    output,
                    input: i,
                },
                None if blocks >= give_up => RoundTrip::Failed(format!(
                    "the click didn't come back on input {} within {GIVE_UP_S} s",
                    i + 1
                )),
                None => RoundTrip::Sent {
                    output,
                    input: i,
                    blocks: blocks + 1,
                },
            },
            other => other,
        };
        if let RoundTrip::Asked { input: i, .. } = self.round_trip {
            self.round_trip_quiet = got(i).iter().all(|v| v.abs() < HEARD);
        }
    }

    /// After a block: the test's output silent, and the click at its first sample once
    /// the input was quiet.
    pub(super) fn round_trip_send(&mut self, out: &mut [Vec<f32>]) {
        match self.round_trip {
            RoundTrip::Asked { output, input, .. } if self.round_trip_quiet => {
                if let Some(o) = out.get_mut(output) {
                    o.fill(0.0);
                    if let Some(first) = o.first_mut() {
                        *first = CLICK;
                    }
                }
                self.round_trip = RoundTrip::Sent {
                    output,
                    input,
                    blocks: 1,
                };
            }
            RoundTrip::Asked { output, .. } | RoundTrip::Sent { output, .. } => {
                if let Some(o) = out.get_mut(output) {
                    o.fill(0.0);
                }
            }
            _ => {}
        }
    }
}
