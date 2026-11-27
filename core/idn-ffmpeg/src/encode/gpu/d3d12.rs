//! A picture on the application's GPU copied straight into a Direct3D 12 encoder's input
//! texture on the same device (spec/extensions/idn-extensions.md §E.27, Windows): a
//! compute pass writes it as NV12, scaled to the encoder's size, into a buffer laid out
//! as Direct3D 12 copies into textures (each plane's placed footprint), and a command
//! list of our own copies that into the texture's two planes, with the transitions the
//! encoder expects (common state before and after), on the application's queue. Nothing
//! comes to the CPU's memory.

use crate::d3d12_gpu::{device_of, frame_layout};
use crate::ffi;
use crate::gpu_convert::err;
use idn_open_protocol::gpu::{GpuHandIn, GpuPicture};
use std::io;
use std::mem::ManuallyDrop;
use wgpu::hal::api::Dx12;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

const NV12_WGSL: &str = r#"
struct P { yoff: u32, yrow: u32, sw: u32, sh: u32, cboff: u32, croff: u32, crow: u32, scw: u32, sch: u32, dw: u32, dh: u32, ypitch: u32, uvoff: u32, uvpitch: u32, p1: u32, p2: u32 };
@group(0) @binding(0) var<storage, read> src: array<u32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> p: P;

fn at(off: u32, row: u32, x: u32, y: u32) -> f32 {
    let i = off + y * row + x;
    return f32((src[i >> 2u] >> ((i & 3u) * 8u)) & 255u);
}

fn sample(off: u32, row: u32, w: u32, h: u32, fx: f32, fy: f32) -> u32 {
    let x = clamp(fx, 0.0, f32(w - 1u));
    let y = clamp(fy, 0.0, f32(h - 1u));
    let x0 = u32(floor(x));
    let y0 = u32(floor(y));
    let x1 = min(x0 + 1u, w - 1u);
    let y1 = min(y0 + 1u, h - 1u);
    let ax = x - f32(x0);
    let ay = y - f32(y0);
    let top = mix(at(off, row, x0, y0), at(off, row, x1, y0), ax);
    let bot = mix(at(off, row, x0, y1), at(off, row, x1, y1), ax);
    return u32(round(clamp(mix(top, bot, ay), 0.0, 255.0)));
}

@compute @workgroup_size(64, 1, 1)
fn main(@builtin(global_invocation_id) g: vec3<u32>) {
    let y = g.y;
    var word = 0u;
    if (g.z == 0u) {
        let x4 = g.x * 4u;
        if (x4 >= p.dw || y >= p.dh) { return; }
        let fy = (f32(y) + 0.5) * f32(p.sh) / f32(p.dh) - 0.5;
        for (var i = 0u; i < 4u; i = i + 1u) {
            let x = min(x4 + i, p.dw - 1u);
            let fx = (f32(x) + 0.5) * f32(p.sw) / f32(p.dw) - 0.5;
            word = word | (sample(p.yoff, p.yrow, p.sw, p.sh, fx, fy) << (8u * i));
        }
        dst[(y * p.ypitch) / 4u + g.x] = word;
    } else {
        let cw = (p.dw + 1u) / 2u;
        let ch = (p.dh + 1u) / 2u;
        let x2 = g.x * 2u;
        if (x2 >= cw || y >= ch) { return; }
        let fy = (f32(y) + 0.5) * f32(p.sch) / f32(ch) - 0.5;
        for (var j = 0u; j < 2u; j = j + 1u) {
            let x = min(x2 + j, cw - 1u);
            let fx = (f32(x) + 0.5) * f32(p.scw) / f32(cw) - 0.5;
            let cb = sample(p.cboff, p.crow, p.scw, p.sch, fx, fy);
            let cr = sample(p.croff, p.crow, p.scw, p.sch, fx, fy);
            word = word | (cb << (16u * j)) | (cr << (16u * j + 8u));
        }
        dst[(p.uvoff + y * p.uvpitch) / 4u + g.x] = word;
    }
}
"#;

pub(crate) struct D3d12Writer {
    hand: GpuHandIn,
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    /// The NV12 picture as the copy takes it (grown when a picture needs more).
    staging: Option<wgpu::Buffer>,
    alloc: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    value: u64,
    event: HANDLE,
}

// SAFETY: wgpu's types are Send and Sync; the Direct3D 12 objects are free-threaded COM
// objects, used by the writer's owner only (the encoder's thread).
unsafe impl Send for D3d12Writer {}

impl Drop for D3d12Writer {
    fn drop(&mut self) {
        // SAFETY: our own event handle.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

fn d3d<T>(r: windows::core::Result<T>, what: &str) -> io::Result<T> {
    r.map_err(|e| err(format!("{what}: {e}")))
}

/// A transition of all of `res`'s subresources.
fn transition(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(res.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Releases the resource reference a barrier holds.
fn release(b: D3D12_RESOURCE_BARRIER) {
    // SAFETY: a transition barrier made by `transition`, its reference ours.
    unsafe {
        let mut t = ManuallyDrop::into_inner(b.Anonymous.Transition);
        ManuallyDrop::drop(&mut t.pResource);
    }
}

impl D3d12Writer {
    /// On `hand`'s device, which must be on Direct3D 12.
    pub(crate) fn new(hand: &GpuHandIn) -> io::Result<D3d12Writer> {
        let device =
            device_of(hand).ok_or_else(|| err("the application's GPU isn't on Direct3D 12"))?;
        // SAFETY: the queue's interface cloned (reference counted), used while it lives.
        let queue = unsafe { hand.queue.as_hal::<Dx12>() }
            .map(|q| q.as_raw().clone())
            .ok_or_else(|| err("the application's queue isn't on Direct3D 12"))?;
        let d = &hand.device;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("into the encoder (D3D12)"),
            source: wgpu::ShaderSource::Wgsl(NV12_WGSL.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty,
            count: None,
        };
        let storage = |read_only| wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("into the encoder (D3D12)"),
            entries: &[
                entry(0, storage(true)),
                entry(1, storage(false)),
                entry(
                    2,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
            ],
        });
        let pl = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("into the encoder (D3D12)"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("into the encoder (D3D12)"),
            layout: Some(&pl),
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let params = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("into the encoder (D3D12)"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // SAFETY: Direct3D 12 calls on a live device.
        unsafe {
            let alloc: ID3D12CommandAllocator = d3d(
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
                "command allocator",
            )?;
            let list: ID3D12GraphicsCommandList = d3d(
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None),
                "command list",
            )?;
            d3d(list.Close(), "command list")?;
            let fence: ID3D12Fence = d3d(device.CreateFence(0, D3D12_FENCE_FLAG_NONE), "fence")?;
            let event = d3d(CreateEventW(None, false, false, None), "event")?;
            Ok(D3d12Writer {
                hand: hand.clone(),
                device,
                queue,
                pipeline,
                layout,
                params,
                staging: None,
                alloc,
                list,
                fence,
                value: 0,
                event,
            })
        }
    }

    /// `pic` copied into the encoder's input `hw` (a Direct3D 12 NV12 texture on this
    /// device) of `size`, the GPU done with it on return.
    pub(crate) fn write(
        &mut self,
        hw: &ffi::Frame,
        pic: &GpuPicture,
        (dw, dh): (usize, usize),
    ) -> io::Result<()> {
        let [y, cb, cr] = &pic.planes[..] else {
            return Err(err("a picture without three planes"));
        };
        // SAFETY: FFmpeg's frame and its texture are live while `hw` is; the GPU's work
        // on them is waited for before this returns.
        unsafe {
            let at = (*hw.0).data[0] as *const u8;
            if at.is_null() {
                return Err(err("an encoder surface without its texture"));
            }
            let layout = frame_layout();
            let tex_ptr = *(at as *const *mut std::ffi::c_void);
            let index = match layout.subresource {
                Some(o) => (*(at.add(o) as *const i32)).max(0) as u32,
                None => 0,
            };
            let texture = ID3D12Resource::from_raw_borrowed(&tex_ptr)
                .ok_or_else(|| err("an encoder surface without its texture"))?
                .clone();
            let desc = texture.GetDesc();
            let layers = u32::from(desc.DepthOrArraySize).max(1);
            // Plane 0 (Y) and plane 1 (CbCr) of slice `index`.
            let subs = [index, index + layers];
            let mut fp = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 2];
            let mut total = 0u64;
            for (k, &s) in subs.iter().enumerate() {
                let base = if k == 0 {
                    0
                } else {
                    total.next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))
                };
                let mut bytes = 0u64;
                self.device.GetCopyableFootprints(
                    &desc,
                    s,
                    1,
                    base,
                    Some(&mut fp[k]),
                    None,
                    None,
                    Some(&mut bytes),
                );
                total = base + bytes;
            }
            let size = total.next_multiple_of(4);
            if self.staging.as_ref().is_none_or(|b| b.size() < size) {
                self.staging = Some(self.hand.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("into the encoder (D3D12)"),
                    size,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }));
            }
            let staging = self.staging.as_ref().expect("made above");
            let (dw, dh) = (dw as u32, dh as u32);
            let words = [
                y.offset as u32,
                y.row_bytes,
                y.width,
                y.height,
                cb.offset as u32,
                cr.offset as u32,
                cb.row_bytes,
                cb.width,
                cb.height,
                dw,
                dh,
                fp[0].Footprint.RowPitch,
                fp[1].Offset as u32,
                fp[1].Footprint.RowPitch,
                0,
                0,
            ];
            let bytes: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            self.hand.queue.write_buffer(&self.params, 0, &bytes);
            let group = self
                .hand
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: pic.buffer.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: staging.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: self.params.as_entire_binding(),
                        },
                    ],
                });
            let mut enc = self.hand.device.create_command_encoder(&Default::default());
            {
                // z 0: luma rows; z 1: chroma rows (half as many, as many words each).
                let mut pass = enc.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &group, &[]);
                pass.dispatch_workgroups(dw.div_ceil(4).div_ceil(64), dh, 2);
            }
            let sub = self.hand.queue.submit([enc.finish()]);
            self.hand
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(sub),
                    timeout: Some(std::time::Duration::from_secs(2)),
                })
                .map_err(err)?;
            // The copy into the texture's planes, on the application's queue.
            let src = staging
                .as_hal::<Dx12>()
                .map(|b| b.raw_resource().clone())
                .ok_or_else(|| err("the staging buffer isn't on Direct3D 12"))?;
            // The last copy done (one that ran past its wait retired this writer, but
            // never reset a list the GPU still runs).
            if self.fence.GetCompletedValue() < self.value {
                return Err(err("the previous copy is still running"));
            }
            d3d(self.alloc.Reset(), "command allocator")?;
            d3d(self.list.Reset(&self.alloc, None), "command list")?;
            let to_copy = transition(
                &texture,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            self.list.ResourceBarrier(std::slice::from_ref(&to_copy));
            for (k, &s) in subs.iter().enumerate() {
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(texture.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: s,
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(src.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: fp[k],
                    },
                };
                self.list.CopyTextureRegion(&dst, 0, 0, 0, &from, None);
                drop(ManuallyDrop::into_inner(dst.pResource));
                drop(ManuallyDrop::into_inner(from.pResource));
            }
            let back = transition(
                &texture,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.list.ResourceBarrier(std::slice::from_ref(&back));
            release(to_copy);
            release(back);
            d3d(self.list.Close(), "command list")?;
            let list: ID3D12CommandList = d3d(self.list.cast(), "command list")?;
            self.queue.ExecuteCommandLists(&[Some(list)]);
            self.value += 1;
            d3d(self.queue.Signal(&self.fence, self.value), "fence")?;
            if self.fence.GetCompletedValue() < self.value {
                d3d(
                    self.fence.SetEventOnCompletion(self.value, self.event),
                    "fence",
                )?;
                // Not done in 2 s: an error (the writer retired, pictures read back), not
                // a half-copied picture encoded.
                if WaitForSingleObject(self.event, 2000) != WAIT_OBJECT_0 {
                    return Err(err("the copy didn't finish within 2 s"));
                }
            }
        }
        Ok(())
    }
}
