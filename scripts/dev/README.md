# Development scripts

Tools for working on the workspace on a Linux build host; none of them is part of a
release. They find the workspace from where they are (`common.sh`) and keep everything
they make — logs, states, results, profiles — in the build kit beside the repository
(`../build-kit`, or `IDN_BUILD_KIT`), with the SDKs and test media that are never in the
repository either. The release programs are `target/release` (or `IDN_BIN`).

| Script | What it does |
|---|---|
| `capped.sh NAME MAXMEM JOBS cargo-args…` | cargo in a memory-capped user scope without swap, logging memory and CPU temperature each second to `build-kit/logs/NAME.mem` (an overrun kills the build, not the machine) |
| `test-all.sh [seq\|par]` | the full test run, debug and release: builds both, then cargo-nextest (each test a process of its own, the programs side by side; `.config/nextest.toml`: no retries, GPU and real-time groups one at a time) and the doc tests; `par` runs the two profiles at once. Summary in `build-kit/test-all/summary.txt`, per-test times in `target/nextest/default/junit.xml`. Needs cargo-nextest (the prebuilt one, `build-kit/nextest`) |
| `prune-target.sh [--delete]` | removes the build folder's leftovers: crates' copies from earlier builds that no current build uses (cargo lists each build's units; the builds are package.sh's, the test builds and the clippy checks, Linux and Windows); nothing a build uses is removed, so nothing is built again. Without `--delete`: what it would remove |
| `panels/voice.sh` | IDN Voice in headless Chromium with a fake microphone (`voice.mjs`): the Industrial Site Manager's `/voice`, the administrator and a guest in one channel, a 2 s talk received over WebRTC, then over the WebSocket (UDP off) |
| `panels/smoke.sh` | the Node panel's smoke test (IDN Media's only web panel): a Node started alone, its panel opened in headless Chromium (`panels/cdp.mjs`: every button clicked, confirm/prompt answered no), failing on any script error, unhandled rejection or console.error; failed requests are listed, not counted |
| `profile/` (kept for new rounds) | `lib.sh`, the GPU samplers (`gpu-sample.sh`, `gpu-procs.sh`) and the Python tools (`startup.py`, `soak.py`, `switcher_state.py`, `industrial_load.py`, `lossy.py`); each round of notes/PERF-QUEUE.md is in `profile/rounds/`, kept to measure again against |
| `profile/lib.sh` | `measure LABEL PID [WARM] [WINDOW]`: CPU, page faults, memory (RSS and the process's own), perf profiles; sourced by the rounds |
| `profile/rounds/round1.sh` … `round9.sh` | the profiling rounds of notes/PERF-QUEUE.md, each with its scenarios (`round3.sh nodeidle`, `round4.sh through`, …); results in `build-kit/prof*/out/results.tsv` |
| `profile/rounds/round7.sh startup\|uhd\|soak [MIN]\|idle\|industrial\|mixer64` | round 7: what starting costs (`startup.py`: ready time, CPU, memory, GPU held), 4K, a held service sampled over time (`soak.py`), programs at rest, IDN Industrial under message and SQL load (`industrial_load.py`), the mixer at 64 channels; `round5.sh` service's start is shared (`service_start`) |
| `profile/rounds/round3-*.sh` | round 3's single scenarios (joining, recording, SpeedHQ CPU/GPU) |
| `profile/rounds/p1-adapt.sh`, `p2-mix.sh` | P1 and P2 of 2026-11-03: a 2160p30 switcher's program, and its vertical mix, to a CPU receiver over the network, with the GPU's load and clocks (`gpu-sample.sh`, `gpu-procs.sh`) and the switcher's frame timing (`switcher_state.py`) |
| `profile/rounds/player_*.sh`, `link_*.sh`, `recv4k.sh`, … | earlier one-scenario measurements (the player, the IDN link under loss, 4K receive) |
| `split/` | moving Rust code into child modules unchanged: `split.py` (an impl's methods), `split2.py` (free functions, whole impls), `sections.py` (line ranges), `tests_out.py` (test modules), `outline.py` (a file's items and sizes) |
| `style/history.py` | takes history tags (dates, lab item and review codes) out of Rust comments, keeping what they explain (STYLE.md's comment rule); a dry run unless `--write` |
