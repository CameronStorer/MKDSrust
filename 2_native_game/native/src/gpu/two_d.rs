//! Raw 2D input upload and compute passes. CPU rendering remains in recomp::gfx2d.
use super::Gpu;
use recomp::{ds_mem::vram_place, machine::RenderSnapshot};

const STRIDE: u32 = 0xca000;
const RAW_SIZE: usize = 0xa6100;

pub struct Renderer2d {
    raw: wgpu::Buffer,
    mapped: wgpu::Buffer,
    maps: wgpu::Buffer,
    mapping: wgpu::ComputePipeline,
    pipeline: wgpu::ComputePipeline,
    backdrop: wgpu::ComputePipeline,
    previous: Vec<u8>,
    pub uploaded: usize,
}

impl Renderer2d {
    pub fn new(gpu: &Gpu) -> Self {
        let buffer = |label, size, usage| gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label), size, usage, mapped_at_creation: false,
        });
        let shader = gpu.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("2D engines"), source: wgpu::ShaderSource::Wgsl(include_str!("two_d.wgsl").into()),
        });
        let pipeline = |entry| gpu.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry), layout: None, module: &shader, entry_point: Some(entry),
            compilation_options: Default::default(), cache: None,
        });
        Self {
            raw: buffer("2D raw", RAW_SIZE as u64, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST),
            mapped: buffer("2D mapped", STRIDE as u64 * 2, wgpu::BufferUsages::STORAGE),
            maps: buffer("2D mappings", 9 * 16, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST),
            mapping: pipeline("map_memory"), pipeline: pipeline("split"), backdrop: pipeline("backdrop"), previous: Vec::new(), uploaded: 0,
        }
    }

    pub fn render(&mut self, gpu: &Gpu, snap: &RenderSnapshot, base: &wgpu::Buffer, other: &wgpu::Buffer, info: &wgpu::Buffer, top: &wgpu::Buffer) {
        let mut bytes = vec![0; RAW_SIZE];
        bytes[..0x1070].copy_from_slice(&snap.io[..0x1070]);
        bytes[0x1100..0x1900].copy_from_slice(&snap.palette[..0x800]);
        bytes[0x1900..0x2100].copy_from_slice(&snap.oam[..0x800]);
        let mut maps = [[0u32; 4]; 9];
        let mut offset = 0x2100;
        for bank in 0..9 {
            let data = &snap.vram[bank];
            bytes[offset..offset + data.len()].copy_from_slice(data);
            let cnt = snap.io[if bank < 7 { 0x240 + bank } else { 0x248 + bank - 7 }];
            let mut target = None;
            let mut len = data.len() as u32;
            if cnt & 0x80 != 0 {
                if let Some(address) = vram_place(bank, cnt & 7, (cnt >> 3) & 3) {
                    for (start, size, to) in [(0x06000000, 0x80000, 0), (0x06400000, 0x40000, 0x80000),
                        (0x06200000, 0x20000, STRIDE), (0x06600000, 0x20000, STRIDE + 0x80000)] {
                        if address >= start && address < start + size {
                            target = Some(to + address - start);
                            len = len.min(start + size - address);
                        }
                    }
                }
                let ext = match (bank, cnt & 7) {
                    (4, 4) => Some((0xc0000, 0x8000)),
                    (5 | 6, 4) => Some((0xc0000 + ((cnt as u32 >> 3) & 1) * 0x4000, 0x4000)),
                    (5 | 6, 5) => Some((0xc8000, 0x2000)),
                    (7, 2) => Some((STRIDE + 0xc0000, 0x8000)),
                    (8, 3) => Some((STRIDE + 0xc8000, 0x2000)),
                    _ => None,
                };
                if let Some((to, size)) = ext { target = Some(to); len = size; }
            }
            if let Some(to) = target { maps[bank] = [to, len, offset as u32, 0]; }
            offset += data.len();
        }
        // Compare against the last submitted frame, not the last emulated frame: skipped frames are safe.
        self.uploaded = 0;
        let mut start = None;
        for at in (0..RAW_SIZE).step_by(256).chain(std::iter::once(RAW_SIZE)) {
            let end = (at + 256).min(RAW_SIZE);
            let dirty = at < RAW_SIZE && (self.previous.is_empty() || bytes[at..end] != self.previous[at..end]);
            if dirty && start.is_none() { start = Some(at); }
            if !dirty {
                if let Some(from) = start.take() {
                    gpu.queue.write_buffer(&self.raw, from as u64, &bytes[from..at]);
                    self.uploaded += at - from;
                }
            }
        }
        self.previous = bytes;
        gpu.queue.write_buffer(&self.maps, 0, bytemuck::cast_slice(&maps));
        let bind = |pipeline: &wgpu::ComputePipeline, entries: &[wgpu::BindGroupEntry]| gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("2D"), layout: &pipeline.get_bind_group_layout(0), entries,
        });
        fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
            wgpu::BindGroupEntry { binding, resource: buffer.as_entire_binding() }
        }
        let mapping = bind(&self.mapping, &[entry(0, &self.raw), entry(1, &self.mapped), entry(2, &self.maps)]);
        let split = bind(&self.pipeline, &[entry(0, &self.raw), entry(1, &self.mapped), entry(3, base), entry(4, other), entry(5, info), entry(6, top)]);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.mapping);
            pass.set_bind_group(0, &mapping, &[]);
            pass.dispatch_workgroups((STRIDE / 4 * 2).div_ceil(256), 1, 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &split, &[]);
            pass.dispatch_workgroups(32, 24, 2);
        }
        gpu.queue.submit([encoder.finish()]);
    }

    /// After [`Renderer2d::render`]: each engine's background layer alone over a 512x512 map period into `back`
    /// ([`BACKDROP_SIZE`] squared u32 per engine, packed RRGGBBAA, master brightness applied).
    pub fn render_backdrop(&self, gpu: &Gpu, back: &wgpu::Buffer) {
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("2D backdrop"), layout: &self.backdrop.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: self.raw.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: self.mapped.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 7, resource: back.as_entire_binding() }],
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.backdrop);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(64, 64, 2);
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// Pixels of one engine's backdrop map period (512 x 512).
pub const BACKDROP_SIZE: u32 = 512;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::{Compositor, compose::upload_layer};

    #[test]
    fn raw_engines_match_reference_with_remapping_and_dirty_uploads() {
        let gpu = Gpu::headless().expect("GPU required for 2D regression");
        let mut compositor = Compositor::new(&gpu, 1);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed as u32
        };
        let mut snap = RenderSnapshot {
            io: vec![0; 0x1070], palette: vec![0; 0x800], oam: vec![0; 0x800],
            vram: recomp::ds_mem::VRAM_BANK_SIZE.iter().map(|&n| (0..n).map(|_| next() as u8).collect()).collect(),
            polys: Vec::new(), viewport: [0,0,255,191],
        };
        let layer: Vec<u32> = (0..256*192).map(|i| (next() & 0xffffff00) | [0,128,255][i % 3]).collect();
        let texture = upload_layer(&gpu, &layer);
        for case in 0..96u32 {
            for b in snap.io.iter_mut().chain(&mut snap.palette).chain(&mut snap.oam) { *b = next() as u8; }
            // Alternate BG/OBJ banks, overlaps, extended palettes, and disabled mappings.
            let controls = [0x81,0x89,0x84,0x84,0x81,0x84,0x85,0x82,0x83];
            for (bank, reg) in [0x240,0x241,0x242,0x243,0x244,0x245,0x246,0x248,0x249].into_iter().enumerate() {
                snap.io[reg] = if case % 3 == 0 { 0x80 | (next() as u8 & 31) } else { controls[bank] };
            }
            for engine in 0..2 {
                let off = engine * 0x1000;
                let display = if case < 90 { 1 } else { case % 4 };
                let d = (next() & 0xfff0ff78) | (display << 16) | 0x1f00 | (case % 6);
                snap.io[off..off+4].copy_from_slice(&d.to_le_bytes());
                // Small signed affine references keep samples within maps frequently.
                for reg in [0x28,0x2c,0x38,0x3c] {
                    let value = (next() as i16 as i32).to_le_bytes();
                    snap.io[off+reg..off+reg+4].copy_from_slice(&value);
                }
                if case % 4 == 0 { snap.io[off+0x6c..off+0x6e].fill(0); }
            }
            snap.io[0x64..0x68].fill(0); // no display capture: this compares one frame's composition
            let (a,b,a_top) = snap.splits();
            compositor.render_raw(&gpu, &snap, &texture);
            assert_eq!(compositor.split_differences(&gpu,&a,&b),0,"case {case}: split records");
            let (a,b) = (a.compose(Some(&layer)),b.compose(None));
            let (top,bottom) = if a_top { (a,b) } else { (b,a) };
            let expected: Vec<_> = top.into_iter().chain(bottom).map(|p| p | 255).collect();
            let actual = gpu.read_rgba(compositor.output());
            let diff = crate::gpu::Diff::of(&actual, &expected);
            assert_eq!(diff.identical, diff.pixels, "case {case}: composition");
            compositor.render_raw(&gpu, &snap, &texture);
            assert_eq!(compositor.uploaded_2d(),0,"unchanged raw inputs");
            // One changed byte must upload only its 256-byte page, even after an identical frame.
            snap.vram[0][1023] ^= 1;
            compositor.render_raw(&gpu, &snap, &texture);
            assert_eq!(compositor.uploaded_2d(),256);
            let (a,b,_) = snap.splits();
            assert_eq!(compositor.split_differences(&gpu,&a,&b),0,"case {case}: dirty page");
        }
    }
}
