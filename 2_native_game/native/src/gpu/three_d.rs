//! GPU 3D renderer (block 2 stage B2.2): draws one [`RenderSnapshot`]'s polygon list on the GPU at `scale` x the
//! console's 256x192.
//!
//! Contract (the software rasterizer `recomp::raster` + `RenderSnapshot::layer3d` is the reference):
//! - Output: an `Rgba8Unorm` texture of 256*scale x 192*scale, top row first, cleared to CLEAR_COLOR
//!   (`io[0x350..0x354]`: RGB555 + 5-bit alpha, channels expanded `(v << 3) | (v >> 2)`, alpha `a * 255 / 31`).
//!   Alpha 0 means "no 3D pixel here" to the 2D compositor, exactly as in the software layer.
//! - Same inputs and rules as `recomp::raster::render`: viewport, near-plane clipping, perspective-correct colour and
//!   texture coordinates, every texture format through `recomp::texture` decoding (decode on the CPU, cache on the
//!   GPU keyed by a stable content ID independent of VRAM addresses), wrap/repeat/flip, modulation and
//!   decal modes, front/back face selection, depth test (less, or equal-within-tolerance), translucent depth writes
//!   only when flagged, polygon alpha blending in submission order, shadow-volume masks and polygon-ID rejection.
//! - At scale 1, `render_compare` must report >= 99% identical pixels per frame and none below 97% against
//!   `RenderSnapshot::layer3d`; remaining differences must be explained (e.g. rounding of interpolation).
use super::Gpu;
use recomp::{gx::Polygon, machine::RenderSnapshot};
use std::{collections::HashMap, path::PathBuf};
#[path = "texture_cache.rs"]
mod texture_cache;
#[path = "texture_pack.rs"]
mod texture_pack;

// Each pixel owns its depth, shadow stencil and two polygon IDs for the entire
// dispatch. This avoids framebuffer readback, inter-draw hazards and restrictions
// of fixed-function stencil/blending (in particular blending onto alpha zero).
// CPU bins retain submission/fan order and use conservative 8x8 native-pixel bounds.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    pos: [f32; 4],
    color: [f32; 4],
    uv: [f32; 4],
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Triangle {
    edges: [[f32; 4]; 3],
    a: Vertex,
    b: Vertex,
    c: Vertex,
    meta: [u32; 4],
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Frame {
    clear: [f32; 4],
    scale: u32,
    /// Tiles per row: 32, plus 2 x margin / 8 for widescreen.
    tiles_x: u32,
    pad: [u32; 2],
}

pub struct Renderer3d {
    scale: u32,
    /// Widescreen: extra columns (DS pixels, a multiple of 8) drawn on each side of the 256-pixel screen.
    margin: u32,
    target: wgpu::Texture,
    pipeline: wgpu::ComputePipeline,
    vertices: wgpu::Buffer,
    bins: wgpu::Buffer,
    uniform: wgpu::Buffer,
    cache: texture_cache::TextureCache,
}
fn buffer(
    device: &wgpu::Device,
    label: &str,
    size: u64,
    usage: wgpu::BufferUsages,
) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}
impl Renderer3d {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
    pub fn new(gpu: &Gpu, scale: u32) -> Renderer3d {
        Self::new_wide(gpu, scale, 0)
    }
    /// A renderer whose layer is `margin` DS pixels wider on each side (widescreen). The game's geometry already
    /// extends past the screen edges, so the extra columns show more of the scene at the same vertical view.
    pub fn new_wide(gpu: &Gpu, scale: u32, margin: u32) -> Renderer3d {
        assert!((1..=8).contains(&scale), "3D scale must be 1-8");
        assert!(margin % 8 == 0 && margin <= 256, "widescreen margin must be a multiple of 8, at most 256");
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("3d layer"),
            size: wgpu::Extent3d {
                width: (256 + 2 * margin) * scale,
                height: 192 * scale,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("3d ordered tile rasterizer"),
                source: wgpu::ShaderSource::Wgsl(include_str!("three_d.wgsl").into()),
            });
        let pipeline = gpu
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("3d ordered tile rasterizer"),
                layout: None,
                module: &shader,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            });
        Renderer3d {
            scale,
            margin,
            target,
            pipeline,
            vertices: buffer(
                &gpu.device,
                "3d triangles",
                1024 * 1024,
                wgpu::BufferUsages::STORAGE,
            ),
            bins: buffer(
                &gpu.device,
                "3d tile lists",
                1024 * 1024,
                wgpu::BufferUsages::STORAGE,
            ),
            uniform: buffer(&gpu.device, "3d frame", 32, wgpu::BufferUsages::UNIFORM),
            cache: texture_cache::TextureCache::new(&gpu.device),
        }
    }
    /// Start/stop asynchronous RGBA8 texture dumping. Changing this setting (or
    /// dropping the renderer) drains the old writer; render calls never wait on PNG I/O.
    pub fn set_texture_dump(&mut self, directory: Option<PathBuf>) {
        self.cache.set_dump(directory);
    }
    /// Index lowercase 16-hex-digit PNG names. Returns the candidate file count;
    /// dimensions and RGBA8 encoding are checked lazily when an ID is first used.
    /// A failed directory read leaves the current pack intact. Reload to retry files.
    pub fn set_texture_pack(&mut self, directory: Option<PathBuf>) -> Result<usize, String> {
        self.cache.set_pack(directory)
    }
    pub fn scale(&self) -> u32 {
        self.scale
    }
    pub fn margin(&self) -> u32 {
        self.margin
    }
    /// Renders `snap`'s 3D frame; the returned texture stays valid until the next call.
    pub fn render(&mut self, gpu: &Gpu, snap: &RenderSnapshot) -> &wgpu::Texture {
        self.render_frame(gpu, snap, None)
    }
    fn render_frame(
        &mut self,
        gpu: &Gpu,
        snap: &RenderSnapshot,
        timestamps: Option<wgpu::ComputePassTimestampWrites<'_>>,
    ) -> &wgpu::Texture {
        let (tex, pal) = snap.texture_memory();
        self.cache.begin_frame(&tex, &pal);
        let mut textures = HashMap::new();
        let mut triangles = Vec::new();
        let tiles_x = (256 + 2 * self.margin as usize) / 8;
        let width = (tiles_x * 8) as f64;
        let mut tiles: Vec<Vec<u32>> = (0..tiles_x * 24).map(|_| Vec::new()).collect();
        let mut epoch = 0;
        let mut previous_mask = false;
        for p in &snap.polys {
            if (p.attr >> 16) & 31 == 0 || p.verts.len() < 3 {
                continue;
            }
            let mask = (p.attr >> 4) & 3 == 3 && (p.attr >> 24) & 63 == 0;
            if mask && !previous_mask {
                epoch += 1;
            }
            previous_mask = mask;
            let screen = project(p, snap.viewport, self.margin as f64);
            if screen.len() < 3 {
                continue;
            }
            let area = -(0..screen.len())
                .map(|i| {
                    let a = screen[i];
                    let b = screen[(i + 1) % screen.len()];
                    a[0] * b[1] - b[0] * a[1]
                })
                .sum::<f64>()
                * 0.5;
            if area == 0.0
                || (area > 0.0 && p.attr & (1 << 7) == 0)
                || (area < 0.0 && p.attr & (1 << 6) == 0)
            {
                continue;
            }
            let (offset, factor) = *textures
                .entry((p.tex_param, p.pltt_base))
                .or_insert_with(|| self.cache.get(p.tex_param, p.pltt_base, &tex, &pal));
            for k in 1..screen.len() - 1 {
                let v = [screen[0], screen[k], screen[k + 1]];
                let det = (v[1][0] - v[0][0]) * (v[2][1] - v[0][1])
                    - (v[2][0] - v[0][0]) * (v[1][1] - v[0][1]);
                if det.abs() < 1e-9 || !v.iter().flatten().all(|x| x.is_finite()) {
                    continue;
                }
                let min_x = v
                    .iter()
                    .map(|v| v[0])
                    .fold(f64::INFINITY, f64::min)
                    .floor()
                    .max(0.0);
                let max_x = v
                    .iter()
                    .map(|v| v[0])
                    .fold(f64::NEG_INFINITY, f64::max)
                    .ceil()
                    .min(width - 1.0);
                let min_y = v
                    .iter()
                    .map(|v| v[1])
                    .fold(f64::INFINITY, f64::min)
                    .floor()
                    .max(0.0);
                let max_y = v
                    .iter()
                    .map(|v| v[1])
                    .fold(f64::NEG_INFINITY, f64::max)
                    .ceil()
                    .min(191.0);
                if min_x > max_x || min_y > max_y {
                    continue;
                }
                let idx = triangles.len() as u32;
                let convert = |v: [f64; 9]| Vertex {
                    pos: [v[0] as f32, v[1] as f32, v[2] as f32, v[3] as f32],
                    color: [v[4] as f32, v[5] as f32, v[6] as f32, 0.0],
                    uv: [v[7] as f32, v[8] as f32, 0.0, 0.0],
                };
                // Form affine planes in f64 before narrowing to GPU floats. Near-plane
                // intersections can project millions of pixels offscreen; subtracting
                // their large barycentric terms in f32 destroys texture precision.
                let dx1 = (v[2][1] - v[0][1]) / det;
                let dy1 = -(v[2][0] - v[0][0]) / det;
                let dx2 = -(v[1][1] - v[0][1]) / det;
                let dy2 = (v[1][0] - v[0][0]) / det;
                let edge1 = [-v[0][0] * dx1 - v[0][1] * dy1, dx1, dy1, 0.0];
                let edge2 = [-v[0][0] * dx2 - v[0][1] * dy2, dx2, dy2, 0.0];
                let edge0 = [1.0 - edge1[0] - edge2[0], -dx1 - dx2, -dy1 - dy2, 0.0];
                let dx: [f64; 9] =
                    std::array::from_fn(|j| (v[1][j] - v[0][j]) * dx1 + (v[2][j] - v[0][j]) * dx2);
                let dy: [f64; 9] =
                    std::array::from_fn(|j| (v[1][j] - v[0][j]) * dy1 + (v[2][j] - v[0][j]) * dy2);
                let origin = std::array::from_fn(|j| v[0][j] - v[0][0] * dx[j] - v[0][1] * dy[j]);
                triangles.push(Triangle {
                    edges: [
                        edge0.map(|v| v as f32),
                        edge1.map(|v| v as f32),
                        edge2.map(|v| v as f32),
                    ],
                    a: Vertex {
                        uv: [origin[7] as f32, origin[8] as f32, factor as f32, 0.0],
                        ..convert(origin)
                    },
                    b: convert(dx),
                    c: convert(dy),
                    meta: [p.attr, p.tex_param, offset, epoch],
                });
                for y in min_y as usize / 8..=max_y as usize / 8 {
                    for x in min_x as usize / 8..=max_x as usize / 8 {
                        tiles[y * tiles_x + x].push(idx);
                    }
                }
            }
        }
        self.cache.upload(&gpu.device, &gpu.queue);
        let mut bins = vec![0u32; tiles.len() + 1];
        for (i, tile) in tiles.iter().enumerate() {
            bins[i] = bins.len() as u32;
            bins.extend(tile);
        }
        let last = tiles.len();
        bins[last] = bins.len() as u32;
        let upload = |dst: &mut wgpu::Buffer, data: &[u8], label| {
            if data.len() as u64 > dst.size() {
                *dst = buffer(
                    &gpu.device,
                    label,
                    data.len().next_power_of_two() as u64,
                    wgpu::BufferUsages::STORAGE,
                );
            }
            if !data.is_empty() {
                gpu.queue.write_buffer(dst, 0, data);
            }
        };
        upload(
            &mut self.vertices,
            bytemuck::cast_slice(&triangles),
            "3d triangles",
        );
        upload(&mut self.bins, bytemuck::cast_slice(&bins), "3d tile lists");
        let cc = u32::from_le_bytes(snap.io[0x350..0x354].try_into().unwrap());
        let e = |shift| {
            let v = (cc >> shift) & 31u32;
            ((v << 3) | (v >> 2)) as f32 / 255.0
        };
        let frame = Frame {
            clear: [
                e(0),
                e(5),
                e(10),
                (((cc >> 16) & 31) * 255 / 31) as f32 / 255.0,
            ],
            scale: self.scale,
            tiles_x: tiles_x as u32,
            pad: [0; 2],
        };
        gpu.queue
            .write_buffer(&self.uniform, 0, bytemuck::bytes_of(&frame));
        let view = self.target.create_view(&Default::default());
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("3d frame"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.vertices.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.bins.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.cache.view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform.as_entire_binding(),
                },
            ],
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("3d tiles"),
                timestamp_writes: timestamps,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.dispatch_workgroups(tiles_x as u32 * self.scale, 24 * self.scale, 1);
        }
        gpu.queue.submit([encoder.finish()]);
        &self.target
    }
}

/// Deliberately the reference's combined near/w test, not a full GPU clip volume:
/// it does not clip the far plane, and intersections use z+w even for w failures.
fn project(p: &Polygon, viewport: [u8; 4], margin: f64) -> Vec<[f64; 9]> {
    let verts: Vec<[f64; 9]> = p
        .verts
        .iter()
        .map(|v| {
            [
                v.clip[0] as f64,
                v.clip[1] as f64,
                v.clip[2] as f64,
                v.clip[3] as f64,
                (v.color & 31) as f64 / 31.0,
                ((v.color >> 5) & 31) as f64 / 31.0,
                ((v.color >> 10) & 31) as f64 / 31.0,
                v.st[0] as f64 / 16.0,
                v.st[1] as f64 / 16.0,
            ]
        })
        .collect();
    let mut clipped = Vec::new();
    for i in 0..verts.len() {
        let a = verts[i];
        let b = verts[(i + 1) % verts.len()];
        let inside = |v: [f64; 9]| v[2] + v[3] >= 0.0 && v[3] > 1e-6;
        if inside(a) != inside(b) {
            let t = (a[2] + a[3]) / (a[2] + a[3] - (b[2] + b[3]));
            clipped.push(std::array::from_fn(|j| a[j] + (b[j] - a[j]) * t));
        }
        if inside(b) {
            clipped.push(b);
        }
    }
    let [x1, y1, x2, y2] = viewport.map(f64::from);
    clipped
        .into_iter()
        .map(|v: [f64; 9]| {
            let iw = 1.0 / v[3];
            [
                margin + x1 + (v[0] / v[3] + 1.0) * 0.5 * (x2 - x1 + 1.0),
                191.0 - (y1 + (v[1] / v[3] + 1.0) * 0.5 * (y2 - y1 + 1.0)),
                (v[2] / v[3] + 1.0) * 0.5,
                iw,
                v[4] * iw,
                v[5] * iw,
                v[6] * iw,
                v[7] * iw,
                v[8] * iw,
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use recomp::gx::Vertex as GxVertex;
    const ATTR: u32 = (31 << 16) | (3 << 6);
    fn snapshot() -> RenderSnapshot {
        RenderSnapshot {
            io: vec![0; 0x2000],
            palette: vec![],
            oam: vec![],
            vram: vec![vec![0; 0x20000]; 9],
            polys: vec![],
            viewport: [0, 0, 255, 191],
        }
    }
    fn triangle(z: i32, color: u16, attr: u32) -> Polygon {
        Polygon {
            verts: [(-3000, -3000), (3000, -3000), (0, 3000)]
                .map(|(x, y)| GxVertex {
                    clip: [x, y, z, 4096],
                    color,
                    st: [0, 0],
                })
                .to_vec(),
            attr,
            tex_param: 0,
            pltt_base: 0,
        }
    }
    fn compare(gpu: &Gpu, renderer: &mut Renderer3d, snap: &RenderSnapshot, label: &str) {
        let actual = gpu.read_rgba(renderer.render(gpu, snap));
        let expected = snap.layer3d();
        let diff = super::super::Diff::of(&actual, &expected);
        assert!(
            diff.percent() >= 99.0,
            "{label}: {diff:?} ({}%)",
            diff.percent()
        );
        // Alpha is a separate compositing contract, not covered by a loose RGB tolerance.
        assert_eq!(
            actual.iter().filter(|p| **p & 255 == 0).count(),
            expected.iter().filter(|p| **p & 255 == 0).count(),
            "{label}: transparent coverage"
        );
    }
    #[test]
    fn gpu_reference_rules() {
        let gpu = match Gpu::headless() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping GPU test: {e}");
                return;
            }
        };
        let mut renderer = Renderer3d::new(&gpu, 1);
        let mut snap = snapshot();
        for alpha in [0, 1, 17, 31] {
            snap.io[0x350..0x354].copy_from_slice(&(0x325fu32 | (alpha << 16)).to_le_bytes());
            compare(&gpu, &mut renderer, &snap, "clear alpha");
        }
        snap.io[0x350..0x354].fill(0);
        let back = triangle(2000, 0x7fff, ATTR | (8 << 24));
        let glass = triangle(0, 0x001f, (ATTR & !(31 << 16)) | (13 << 16) | (3 << 24));
        for polygons in [
            vec![glass.clone()],
            vec![back.clone(), glass.clone(), glass.clone()],
            vec![
                back.clone(),
                glass.clone(),
                triangle(-1000, 0x7c00, glass.attr ^ ((3 ^ 4) << 24)),
            ],
        ] {
            snap.polys = polygons;
            compare(&gpu, &mut renderer, &snap, "translucent IDs");
        }
        for caster in [3, 8] {
            let receiver = triangle(0, 0x7fff, ATTR | (caster << 24));
            let mask = triangle(1000, 0, ATTR | (3 << 4));
            let shade = triangle(-1000, 0, glass.attr | (3 << 4));
            snap.polys = vec![
                receiver,
                mask.clone(),
                shade.clone(),
                triangle(-2000, 0, mask.attr),
                triangle(-3000, 0, shade.attr ^ ((3 ^ 4) << 24)),
            ];
            compare(
                &gpu,
                &mut renderer,
                &snap,
                "shadow groups and caster rejection",
            );
        }
        for flags in [0, 1 << 14, 1 << 11] {
            snap.polys = vec![
                back.clone(),
                triangle(2000, 0x7c00, glass.attr | flags),
                triangle(2001, 0x001f, ATTR | flags),
            ];
            compare(
                &gpu,
                &mut renderer,
                &snap,
                "depth equality and translucent writes",
            );
        }
        snap.io[0x240] = 0x83;
        snap.io[0x244] = 0x83;
        for (i, b) in snap.vram[0].iter_mut().enumerate() {
            *b = (i.wrapping_mul(37) >> 1) as u8;
        }
        for (i, b) in snap.vram[4].iter_mut().enumerate() {
            *b = (i.wrapping_mul(13)) as u8;
        }
        for format in 1..=7 {
            for mode in [0, 1] {
                let mut p = triangle(0, 0x3def, ATTR | (mode << 4));
                p.tex_param = (format << 26) | (1 << 16) | (1 << 18) | (1 << 29);
                p.verts[0].st = [-200, -200];
                p.verts[1].st = [400, 20];
                p.verts[2].st = [20, 400];
                snap.polys = vec![p];
                compare(&gpu, &mut renderer, &snap, "texture formats and wrap");
                snap.vram[4][2] ^= 31;
                compare(&gpu, &mut renderer, &snap, "palette invalidation");
            }
        }
        snap.polys = vec![triangle(0, 0x7fff, ATTR)];
        snap.polys[0].verts[0].clip[2] = -8000;
        compare(&gpu, &mut renderer, &snap, "near clipping");
        // A synthetic eye-crossing triangle: near clipping creates enormous
        // projected coordinates. Interpolating narrowed vertices loses texels.
        let mut near = triangle(0, 0x7fff, ATTR);
        for (v, (clip, uv)) in near.verts.iter_mut().zip([
            ([260000, -260000, -3000, -1000], [4000, 1000]),
            ([74000, 55000, 138000, 140000], [0, -4600]),
            ([-56000, -61000, 86000, 88000], [4000, -4600]),
        ]) {
            v.clip = clip;
            v.st = uv;
        }
        near.tex_param = (7 << 26) | (3 << 16);
        snap.polys = vec![near];
        compare(
            &gpu,
            &mut renderer,
            &snap,
            "eye crossing interpolation planes",
        );
        snap.viewport = [21, 37, 231, 171];
        compare(&gpu, &mut renderer, &snap, "viewport");
        snap.polys[0].attr &= !(3 << 6);
        compare(&gpu, &mut renderer, &snap, "culling");
        // At scale 3 the centre sample of every 3x3 block is exactly a scale-1 sample.
        snap.polys = vec![triangle(0, 0x7fff, ATTR)];
        let low = gpu.read_rgba(renderer.render(&gpu, &snap));
        let mut high_renderer = Renderer3d::new(&gpu, 3);
        let high = gpu.read_rgba(high_renderer.render(&gpu, &snap));
        for y in 0..192 {
            for x in 0..256 {
                assert_eq!(low[y * 256 + x], high[(y * 3 + 1) * 768 + x * 3 + 1]);
            }
        }
    }

    #[test]
    fn widescreen_keeps_the_centre_and_extends_the_sides() {
        let gpu = Gpu::headless().expect("GPU required for the widescreen regression");
        let mut snap = snapshot();
        // A triangle wider than the screen: its corners lie past both side edges.
        let mut wide = triangle(0, 0x03e0, ATTR);
        wide.verts[0].clip[0] = -6000;
        wide.verts[1].clip[0] = 6000;
        snap.polys = vec![triangle(1000, 0x7c1f, ATTR), wide];
        let normal = gpu.read_rgba(Renderer3d::new(&gpu, 2).render(&gpu, &snap));
        let (margin, scale) = (40usize, 2usize);
        let mut renderer = Renderer3d::new_wide(&gpu, scale as u32, margin as u32);
        let widened = gpu.read_rgba(renderer.render(&gpu, &snap));
        let (w, nw) = ((256 + 2 * margin) * scale, 256 * scale);
        assert_eq!(widened.len(), w * 192 * scale);
        // Identical up to single pixels where an edge passes exactly through a pixel centre (the shifted x rounds
        // that tie differently).
        let differing = (0..192 * scale)
            .flat_map(|y| (0..nw).map(move |x| (y, x)))
            .filter(|&(y, x)| widened[y * w + margin * scale + x] != normal[y * nw + x])
            .count();
        assert!(differing * 2000 <= nw * 192 * scale, "{differing} centre pixels differ");
        // The side columns show the part of the triangle the 4:3 screen cut off.
        let row = 150 * scale;
        assert_ne!(widened[row * w + 10] & 255, 0, "left side drawn");
        assert_ne!(widened[row * w + w - 10] & 255, 0, "right side drawn");
    }

    #[test]
    fn nearest_pack_is_exact_at_scales_one_and_two() {
        let gpu = Gpu::headless().expect("GPU required for texture pack regression");
        let root = std::env::temp_dir().join("codex_b2_tex");
        std::fs::create_dir_all(&root).unwrap();
        let dump = tempfile::tempdir_in(&root).unwrap();
        let pack = tempfile::tempdir_in(&root).unwrap();
        let mut snap = snapshot();
        snap.io[0x240] = 0x83;
        snap.io[0x244] = 0x83;
        for (i, b) in snap.vram[0].iter_mut().enumerate() {
            *b = i.wrapping_mul(37) as u8;
        }
        for (i, b) in snap.vram[4].iter_mut().enumerate() {
            *b = i.wrapping_mul(13) as u8;
        }
        let mut scenes = Vec::new();
        for format in 1..=7 {
            for wrap in [0, 3 << 16, (3 << 16) | (3 << 18)] {
                let mut p = triangle(0, 0x7fff, ATTR);
                p.tex_param = (format << 26) | wrap | (1 << 29);
                p.verts[0].st = [-511, -257];
                p.verts[1].st = [777, 33];
                p.verts[2].st = [19, 799];
                scenes.push(p);
            }
        }
        let mut r = Renderer3d::new(&gpu, 1);
        // Toggle dumping after caching: existing entries must still be dumped.
        snap.polys = vec![scenes[0].clone()];
        r.render(&gpu, &snap);
        r.set_texture_dump(Some(dump.path().into()));
        for p in &scenes {
            snap.polys = vec![p.clone()];
            r.render(&gpu, &snap);
        }
        r.set_texture_dump(None);
        let files: Vec<_> = std::fs::read_dir(dump.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 7, "wrap modes share IDs");
        for path in files {
            let (w, h, px) = texture_pack::read_png(&path).unwrap();
            let mut up = Vec::new();
            for y in 0..h * 2 {
                for x in 0..w * 2 {
                    up.push(px[((y / 2) * w + x / 2) as usize]);
                }
            }
            texture_pack::write_png(
                &pack.path().join(path.file_name().unwrap()),
                w * 2,
                h * 2,
                &up,
            )
            .unwrap();
        }
        for scale in [1, 2] {
            let mut original = Renderer3d::new(&gpu, scale);
            let mut replaced = Renderer3d::new(&gpu, scale);
            assert_eq!(
                replaced.set_texture_pack(Some(pack.path().into())).unwrap(),
                7
            );
            for p in &scenes {
                snap.polys = vec![p.clone()];
                let expected = gpu.read_rgba(original.render(&gpu, &snap));
                let actual = gpu.read_rgba(replaced.render(&gpu, &snap));
                assert_eq!(actual, expected, "scale {scale}, reg {:08x}", p.tex_param);
            }
            replaced.set_texture_pack(None).unwrap();
            assert_eq!(
                gpu.read_rgba(original.render(&gpu, &snap)),
                gpu.read_rgba(replaced.render(&gpu, &snap))
            );
            // A pack is truly sampled, including alpha-zero rejection.
            let (tex, pal) = snap.texture_memory();
            let p = recomp::texture::TexParams::from_reg(snap.polys[0].tex_param);
            let id = texture_cache::fingerprint(&p, 0, &tex, &pal);
            let path = pack.path().join(format!("{id:016x}.png"));
            std::fs::remove_file(&path).unwrap();
            texture_pack::write_png(&path, 16, 16, &[0; 256]).unwrap();
            replaced.set_texture_pack(Some(pack.path().into())).unwrap();
            assert!(
                gpu.read_rgba(replaced.render(&gpu, &snap))
                    .iter()
                    .all(|p| *p == 0)
            );
            // Restore for the next renderer scale.
            let px = recomp::texture::decode(&p, 0, &tex, &pal);
            let up: Vec<_> = (0..256)
                .map(|i| px[(i / 16 / 2) * 8 + i % 16 / 2])
                .collect();
            std::fs::remove_file(&path).unwrap();
            texture_pack::write_png(&path, 16, 16, &up).unwrap();
        }
    }

    #[test]
    fn replacement_detail_obeys_fractional_wrap_flip_and_clamp() {
        let gpu = Gpu::headless().expect("GPU required for replacement sampling test");
        let root = std::env::temp_dir().join("codex_b2_tex");
        std::fs::create_dir_all(&root).unwrap();
        let pack = tempfile::tempdir_in(root).unwrap();
        let mut snap = snapshot();
        snap.io[0x240] = 0x83;
        snap.vram[0][..128].fill(255);
        let p = recomp::texture::TexParams::from_reg(7 << 26);
        let (tex, pal) = snap.texture_memory();
        let id = texture_cache::fingerprint(&p, 0, &tex, &pal);
        let pixels: Vec<u32> = (0..256)
            .map(|i| ((i % 16 * 16) << 24) | ((i / 16 * 16) << 16) | 255)
            .collect();
        texture_pack::write_png(&pack.path().join(format!("{id:016x}.png")), 16, 16, &pixels)
            .unwrap();
        let mut r = Renderer3d::new(&gpu, 1);
        r.set_texture_pack(Some(pack.path().into())).unwrap();
        for wrap in [0, 3 << 16, (3 << 16) | (3 << 18)] {
            for st in [
                -257i32, -129, -128, -127, -17, -16, -15, -1, 0, 1, 7, 8, 15, 16, 127, 128, 129,
                255, 256,
            ] {
                let mut poly = triangle(0, 0x7fff, ATTR);
                poly.tex_param = (7 << 26) | wrap;
                for v in &mut poly.verts {
                    v.st = [st as i16, -st as i16];
                }
                snap.polys = vec![poly];
                let mut high = recomp::texture::TexParams::from_reg(snap.polys[0].tex_param);
                high.width *= 2;
                high.height *= 2;
                let (x, y) = recomp::texture::wrap(&high, st.div_euclid(8), (-st).div_euclid(8));
                let actual = gpu.read_rgba(r.render(&gpu, &snap));
                assert_eq!(
                    actual[96 * 256 + 128],
                    pixels[y * 16 + x],
                    "wrap {wrap:x}, st {st}"
                );
            }
        }
    }

    /// Run explicitly with a local snapshot path; no game data is stored in tests.
    #[test]
    #[ignore = "set MKDS_GPU_BENCH_SNAPSHOT to a local race snapshot"]
    fn benchmark_race() {
        let gpu = match Gpu::headless() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Skipping GPU benchmark: {e}");
                return;
            }
        };
        if !gpu
            .adapter
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
        {
            eprintln!("Skipping GPU benchmark: adapter lacks timestamp queries");
            return;
        }
        let (device, queue) =
            pollster::block_on(gpu.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("3d benchmark"),
                required_features: wgpu::Features::TIMESTAMP_QUERY,
                required_limits: wgpu::Limits::default().using_resolution(gpu.adapter.limits()),
                ..Default::default()
            }))
            .unwrap();
        let gpu = Gpu {
            adapter: gpu.adapter,
            device,
            queue,
        };
        let path = std::env::var("MKDS_GPU_BENCH_SNAPSHOT").expect("local snapshot path");
        let snap = RenderSnapshot::decode(&std::fs::read(path).unwrap()).unwrap();
        let mut renderer = Renderer3d::new(&gpu, 1);
        if let Some(pack) = std::env::var_os("MKDS_GPU_BENCH_PACK") {
            renderer.set_texture_pack(Some(pack.into())).unwrap();
        }
        for _ in 0..10 {
            renderer.render(&gpu, &snap);
        }
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let queries = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("3d timings"),
            ty: wgpu::QueryType::Timestamp,
            count: 400,
        });
        let resolve = buffer(
            &gpu.device,
            "3d resolved timings",
            3200,
            wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        );
        let readback = buffer(
            &gpu.device,
            "3d timing readback",
            3200,
            wgpu::BufferUsages::MAP_READ,
        );
        let start = std::time::Instant::now();
        for i in 0..200 {
            renderer.render_frame(
                &gpu,
                &snap,
                Some(wgpu::ComputePassTimestampWrites {
                    query_set: &queries,
                    beginning_of_pass_write_index: Some(i * 2),
                    end_of_pass_write_index: Some(i * 2 + 1),
                }),
            );
        }
        let cpu_ms = start.elapsed().as_secs_f64() * 1000.0 / 200.0;
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.resolve_query_set(&queries, 0..400, &resolve, 0);
        encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, 3200);
        gpu.queue.submit([encoder.finish()]);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.unwrap());
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        let data = readback.slice(..).get_mapped_range().unwrap();
        let ticks: &[u64] = bytemuck::cast_slice(&data);
        let mut times: Vec<f64> = ticks
            .chunks_exact(2)
            .map(|t| (t[1] - t[0]) as f64 * gpu.queue.get_timestamp_period() as f64 / 1e6)
            .collect();
        times.sort_by(f64::total_cmp);
        eprintln!(
            "GPU {:?}; {} polygons; warm GPU mean {:.3} ms, p95 {:.3} ms; CPU preparation/submission {:.3} ms/frame",
            gpu.adapter.get_info(),
            snap.polys.len(),
            times.iter().sum::<f64>() / 200.0,
            times[190],
            cpu_ms
        );
    }
}
