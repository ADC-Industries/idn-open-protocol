# idn-shq-gpu

Reconstructs SpeedHQ pictures on the GPU. The CPU reads the bitstream into coefficients
(`idn_open_protocol::shq::CoefPicture`, which a receiver gets from
`input::InputConfig::coefficients`), and the GPU does the inverse DCT and writes the
samples. The inverse DCT was about half of a 2160p picture's CPU decode, so moving it to
the GPU is most of the saving. Through wgpu (Vulkan, DX12 or Metal); MIT OR Apache-2.0.

Two callers share it: SpeedHQ receivers and the OBS plugin read the samples back to the
CPU (`Reconstructor`); the switcher's compositor keeps them on the GPU (`idct`).

## Which API

- **`Reconstructor` — samples back to the CPU.** For a receiver that hands pictures on in
  memory. `Reconstructor::new(gpu)`, then `reconstruct(&CoefPicture) -> Result<Picture,
  String>`. The `Picture` is exactly what `shq::reconstruct` produces on the CPU (bit for
  bit in f64; in f32, a sample may be one step off). `recycle(picture)` hands its plane
  buffers back to be filled again, so a steady stream doesn't reallocate. An `Err` means
  the GPU can't be relied on (a lost or out-of-memory device): reconstruct on the CPU from
  then on.

- **`idct::Recon` — samples stay on the GPU.** The lower-level building block.
  `Recon::new(&wgpu::Device, Precision)`, then `record(&device, &mut encoder, &coefs,
  &pics, &jobs)` records the inverse-DCT compute pass into a command encoder you own,
  reading a coefficient buffer and writing a samples buffer you provide (pack the
  coefficients with `idct::pack` / `pack_into`). This is what the switcher's compositor
  uses to keep pictures on the GPU; use it when you have your own wgpu device and pipeline
  — see "The device" below.

## The device, and one per process

`Reconstructor` runs on a shared `Gpu`. `Gpu::shared(env) -> Option<Arc<Gpu>>` opens the
process's device on first use and returns that same one every time (`None` if there is no
usable, non-software GPU); `env` names the variable that forces f32 (below). `Gpu::open`
makes an unshared one.

**There is one wgpu device per process, by design.** Some Vulkan loaders race when a
process stands up more than one device, so `Gpu::shared` is a singleton and every
`Reconstructor` uses it. If your host application already has a wgpu device, don't stand up
a second one for SpeedHQ work — give the crate yours:

- **Hand your device in:** `Gpu::adopt(device, queue, env)` wraps a clone of your own
  `Device`/`Queue` (both are cheap, reference-counted handles) as a `Gpu`; build
  `Reconstructor`s on an `Arc` of it. The one device in the process is then yours, and the
  samples come back to the CPU exactly as with `shared`. An adopted device keeps *your*
  uncaptured-error handler (an opened one gets the crate's); `reconstruct` still returns
  `Err` on a submission or read-back failure.
- **Or keep the pictures on your device:** drive `idct::Recon::new(&your_device, precision)`
  directly and record the pass yourself (as the switcher's compositor does), so the samples
  never leave the GPU.
- **No device of your own?** `Gpu::shared(env)` opens and owns the single process device
  for you.

## Arithmetic (f64 / f32)

`Precision::F64` matches the CPU exactly; `Precision::F32` is for GPUs without f64 shaders
(a sample one step off now and then). `Precision::for_features(device.features(), env)`
chooses f64 where the device has it, unless the environment variable named by `env` is
`f32` — the OBS plugin reads `IDN_OBS_IDCT`, the switcher `IDN_SWITCHER_IDCT`.
`Gpu::precision` and `Gpu::has_f64` report what a device does; one without f64 is forced to
f32 regardless of what you ask for.

## Output format and colour

`reconstruct` yields an `shq::Picture`: **planar Y'CbCr**, not RGB and not an sRGB texture.
Per `shq::Plane`, the samples are **limited range** (Y 16–235, Cb/Cr 16–240, 128 = neutral)
at 8 bits; a `high_precision` (10-bit/HDR) source can be read at 11 bits
(`Picture::precise`, stored as `value << 5` in 16-bit planes). Alpha, when the layout has
it, is a **separate plane, straight (not premultiplied)**, 0–255. The chroma layout —
4:2:2, 4:2:0, 4:4:4, with or without alpha — is `Picture::fourcc`. Convert to RGB and
apply the transfer you need yourself; the samples are the video's own non-linear
(gamma-encoded) Y'CbCr, BT.709 for SDR.

## Synchronisation and ownership

`Reconstructor::reconstruct` submits the work and blocks until the samples are mapped back,
so you receive a finished `Picture` and own it — there is no fence for you to manage. The
`idct::Recon` path hands you a recorded pass instead: you submit it on your own queue,
order it against your render pass yourself, and own the lifetime of the `coefs`/`pics`
buffers. The shared `Gpu` installs an uncaptured-error handler, so a device error comes
back to you as an `Err` rather than panicking wgpu's default handler.

## Build

`cargo build -p idn-shq-gpu`. Requires **wgpu `=30.0.1`** (pinned; the switcher pins the
same version, so an app embedding both agrees on one wgpu). wgpu pulls the platform's
Vulkan, DX12 or Metal backend. There are no codecs here — only the inverse DCT; reading
the bitstream into coefficients is in `idn-open-protocol` (`shq`).
