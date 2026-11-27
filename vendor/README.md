# Vendored crates

## quinn-proto 0.11.19

Upstream: https://github.com/quinn-rs/quinn (MIT OR Apache-2.0; licence files kept in
`quinn-proto/`). Copied unchanged from the crates.io release except for one patch in
`src/connection/packet_builder.rs`, marked "IDN Open Protocol patch":

quinn performs a first, routine 1-RTT key update after a random 10–1000 packets. The NDI
QUIC profile (spec chapter 4, §4.2.3) sends 1-RTT packets unprotected and never updates
keys, and NDI peers have never been seen to accept a key-phase change. The patch skips
the routine update when the current 1-RTT key reports no confidentiality limit
(`u64::MAX`), which only the NDI stub keys do. Real TLS keys (secure QUIC, `squic`) keep
quinn's behaviour.

Used through `[patch.crates-io]` in `Cargo.toml`. To update quinn, copy the new release
here and re-apply the patch.

## ASIO

IDN's ASIO support comes from its own crate `core/idn-asio-abi` (MIT OR Apache-2.0),
written from cwASIO's public headers (cwASIO is MIT); it contains nothing of Steinberg's
ASIO SDK.

## cpal 0.18.2

Upstream: https://github.com/RustAudio/cpal (Apache-2.0; licence in `cpal/`). Copied
unchanged from the crates.io release except for one patch in `src/host/alsa/mod.rs`,
marked "IDN Open Protocol patch": with a fixed buffer size cpal sets every ALSA stream's
ring to exactly two periods. For capture at 1-3 ms periods that overruns whenever the
reading thread is one period late (lab item 115: at 2 ms blocks on snd-aloop a third of
the input was lost). Capture streams now get at least 4 periods and at least 20 ms,
which adds no latency (each period is still read as it completes); playback keeps two
periods, where the ring is latency. Used through `[patch.crates-io]`. To update cpal, copy the new release
here and re-apply the patch.

## libvmx (OMT's VMX codec)

Upstream: https://github.com/openmediatransport/libvmx at commit 31724bb (MIT; licence in
`libvmx/LICENSE.txt`). C++ sources only, unchanged (see `libvmx/README.md`); compiled into
`idn-omt` by its build script, with libstdc++ linked statically on GNU targets.

## whisper-rs-sys 0.15.0

Upstream: https://github.com/tazz4843/whisper-rs (Unlicense; whisper.cpp inside it is MIT).
Copied unchanged from the crates.io release except `build.rs`, marked "IDN Open Protocol
patch": on Windows with MinGW, ggml's CMake names its archives `ggml.a` (no `lib`
prefix), so they are linked by file name, and the C++ runtime is linked statically
(Windows has no libstdc++-6.dll). Used by `idn-captions` through `[patch.crates-io]`.

A second patch, for the Vulkan backend (`idn-captions`' `gpu` feature), in
`whisper.cpp/ggml/src/ggml-vulkan/ggml-vulkan.cpp` and its `CMakeLists.txt` and in
`build.rs`, all marked "IDN Open Protocol patch": the Vulkan loader (`vulkan-1.dll`,
`libvulkan.so.1`) is opened at run time with Vulkan-Hpp's `DynamicLoader` instead of being
linked. A machine without a GPU driver (a server, a VM) then still starts every program;
ggml's Vulkan backend just isn't there and whisper runs on the CPU. The direct calls
(`vkGetInstanceProcAddr`, `vkCmdCopyBuffer`, `vkGetPhysicalDeviceFeatures2`) go through
ggml's own dispatcher instead.
Building needs only the Vulkan headers and `glslc` (Ubuntu: `glslc`, `libvulkan-dev`);
`Vulkan_INCLUDE_DIR` and `Vulkan_GLSLC_EXECUTABLE` in the environment are passed to CMake
(cross-builds); ggml's Vulkan part is built when those two are found (upstream: when the
loader library is). `build.rs` also copies `whisper.cpp` into the build again whenever it
changes (upstream: only the first time, so an edit here never reached a build). To update
whisper-rs-sys, copy the new release here and re-apply all three.

A third patch in `ggml-vulkan.cpp` ("IDN Open Protocol patch: wait"): `ggml_vk_wait_for_fence`
waits for a graph's last part in `waitForFences` instead of spinning on the fence's
status. Live captions run one pass a second; the spin kept a core busy for each pass's
end (captions at 60% of a core with base.en on the GPU, 16% with the patch; profiling
2026-10-31).

A fourth patch in `whisper.cpp/src/whisper.cpp` ("IDN Open Protocol patch: mel cache"):
the state keeps the last spectrogram's samples and its frames' values before clamping.
Live captions run a pass a second over the line so far, so each pass recomputed the
whole line's spectrogram. A frame whose 400-sample window lay wholly within the last
pass's samples, when those samples are unchanged at the start of this pass's (compared
byte for byte), is taken from the cache; only the frames the new samples reach are
computed. The clamping and scaling over the whole spectrogram run as before, so the
spectrogram is the same, and so are the captions (checked: every draft and final line
of a 34 s speech file identical). The spectrogram went from about 20% of the captions'
CPU to about 6% (2026-11-02).

## rubato 0.16.2

Upstream: https://github.com/HEnquist/rubato (MIT; `LICENSE.txt` kept). Copied unchanged
from the crates.io release except `src/asynchro_sinc.rs`, `SincFixedIn` only, marked
"IDN Open Protocol patch": its process walks a list of the active channels' indexes,
made once a chunk from the channel mask, instead of every channel and its flag for each
output sample, and shifts only the active channels' filter history (an inactive one's is
silence: our resampler flushes zeros through before a channel goes inactive). A routed
lane on a 32-channel desk feeds two of its channels: the flag checks for the other 30 were
the desk's largest cost (desk pass 2026-11-02). Each active channel's arithmetic is
unchanged: a test (`audio::resample`) checks a channel comes out the same alone or with
others. Used through `[patch.crates-io]`. To update rubato, copy the new release here and
re-apply the patch.

