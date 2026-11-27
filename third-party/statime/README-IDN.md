# statime (IDN's copy)

The `statime` crate (Precision Time Protocol, Tweede Golf and contributors,
<https://github.com/pendulum-project/statime>), release 0.4.0, with the PTP v1
(IEEE 1588-2002) follower support that Teodor Woźniak added for Inferno, from
<https://github.com/teodly/statime>, branch `inferno-dev`, commit 244f20a (2025):
only the library (`statime/`), not its daemon, its `timestamped-socket` or its
`clock-steering` forks.

Licence unchanged: Apache-2.0 OR MIT (COPYRIGHT, LICENSE-APACHE, LICENSE-MIT here).

Why (roadmap R.31): Dante equipment without AES67 keeps time with
PTP v1. With v1 in the site clock, a Node follows a Dante network's leader, so site
time is that network's time and `idn-aoip` needs no clock daemon of its own.

IDN's changes to this copy are listed below as they are made.

1. Cargo.toml written out (statime's workspace values filled in); `rand`'s `std`
   features as a dev-dependency, so the `Port` example in `src/port/mod.rs` compiles
   (statime's own workspace gave them).
2. `messages_v1/follow_up.rs`: Follow_Up written as the standard lays it out and as the
   fork itself reads it (2 reserved bytes, the sequence id, the timestamp); the fork
   wrote the id at 0 and the timestamp at 2. (IDN's v1 leader, change 5, sends it.)
3. `Cargo.toml`: the `unused` and `mismatched_lifetime_syntaxes` warnings off (the fork's
   v1 code has unused items; left as it is, to stay close to it).
4. A v1 clock variance (signed) ranked as v2's (unsigned) by shifting it, not casting it
   (`common::v1_variance_to_v2`, in `port/bmca.rs` and `bmc/dataset_comparison.rs`): a
   cast put every usual negative variance after every positive one, so a clock offering
   the largest variance could outrank real equipment.
5. **A v1 leader** (the fork had none): `messages_v1` builds the two-step Sync (PTP_ASSIST
   set; the grandmaster's v1 properties passed on, or this clock's as stratum 4 `DFLT`
   with its variance), the Follow_Up and the Delay_Resp; `port/master.rs` sends Sync and
   Follow_Up while leading and answers v1 Delay_Req; v1 sends no Announce (its timer
   keeps its pace). Written from IEEE 1588-2002's message layout.
6. From upstream statime (pendulum-project, after 0.4.0): commit ff18563,
   `PtpInstance::set_clock_quality` (Blair Hagen), with its tests; one test line dropped
   (it set an `InstanceConfig::clock_quality` field from a later upstream commit not
   taken).
7. `port/bmca.rs`: the fork wrote the master's identity to a predictable /tmp file on every
   move to following (Inferno's debugging; a local user could plant a link there):
   removed.
8. `parent_ds.grandmaster_v1` kept in step with the parent: cleared when this clock leads
   or follows a v2 master, set when it follows a v1 one (stale v1 grandmaster data was
   passed on after a change of leader).
9. **The bridge's v1 side** (`bmc/bmca.rs`, `common::v1_rank_of_v2`): a v2 grandmaster
   relayed into v1 is ranked as v1 ranks (preferred when its priority1 is under 128 or
   its class is a locked one; stratum 2 for such a class, else 3; its variance kept
   below an IDN Node's own offer), so v1 clocks order it as the bridge did; and the v1
   port stays passive next to a live v1 leader that wouldn't yield to it (both ports are
   on one network in IDN's bridge), logging "two time domains", rather than both
   leading.
10. `port/slave.rs`: a v1 Follow_Up matched to its Sync by associatedSequenceId (a leader
    may number Follow_Up separately).
11. A v1 Delay_Req answered only when it names this port as its parent.
12. **v1 subdomains**: domain 0-3 as `_DFLT`, `_ALT1`-`_ALT3`; a v1 port takes only its
    own subdomain and writes it in what it sends.
13. The v1 leader's Sync: its rough origin time (not 0, which some followers take for
    1970), the grandmaster's leap-second flags, this clock's own local stratum and
    variance, the grandmaster's MAC address as its UUID (an EUI-64 identity with ff-fe
    in its middle gives the MAC back), and its own Sync sequence id as the grandmaster's
    while it is the grandmaster. A Delay_Req's variance shifted, not cast. One packet
    buffer size for both versions (v2's 1024).
14. **Bug run H (2026-11-15)**:
    - Ebest (`ptp_instance.rs`, `Bmca::find_best_of_ports`) leaves out a v1 message whose
      grandmaster is a v2 one another port hears directly: another bridge's relay isn't
      a leader of its own. Taken as one, its relayed rank (preferred: priority1 64)
      outranked the grandmaster, two bridges followed each other in a loop, and the
      grandmaster went passive. Of bridges relaying one grandmaster, the lowest UUID
      relays; the others' v1 ports are passive.
    - "Two time domains" counted in `TWO_TIME_DOMAINS` for the program to show (the
      `log` line was shown by no IDN program); now a debug line.
    - A port forced to lead by its receipt timer after following leads as itself, as
      M1/M2 set it (its Syncs named the dead leader as grandmaster).
    - Steps removed + 1 saturating (a Sync claiming 65535 overflowed).
    The v1 subdomains' own multicast addresses are the program's (idn-open-protocol's
    `ClockConfig::v1_group`).
15. **Bug run J (2026-11-17)**: as a v1 leader of its own (`messages_v1::grandmaster_props`),
    `preferred` is set when its priority1 is below 128: a clock set by hand to lead is
    followed by v1 equipment, which compares the preferred flag first (before, never
    preferred: such a clock and a Dante-compatible leader both led).
