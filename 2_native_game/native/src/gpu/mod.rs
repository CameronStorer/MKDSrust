//! GPU side of the native platform layer (block 2, `https://github.com/CameronStorer/MKDSrust-Docs`): one `wgpu` device shared by the
//! 3D renderer ([`three_d`]), raw 2D engines ([`two_d`]), screen compositor and presenter. The software renderer in `recomp`
//! stays the reference: every GPU stage is compared against it with `render_compare`.
pub mod compose;
pub mod three_d;
pub mod two_d;

pub use compose::Compositor;
pub use three_d::Renderer3d;

/// Packed `0xRRGGBBAA`, the software renderer's pixel format.
pub type Rgba = u32;

pub struct Gpu {
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// A device without a window (tests, `render_compare`).
    pub fn headless() -> Result<Gpu, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| format!("No graphics adapter: {e}"))?;
        Self::from_adapter(adapter)
    }

    pub fn from_adapter(adapter: wgpu::Adapter) -> Result<Gpu, String> {
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("mkds"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("Cannot open the graphics device: {e}"))?;
        Ok(Gpu { adapter, device, queue })
    }

    /// Reads an `Rgba8Unorm` texture back as packed `0xRRGGBBAA` rows (tests and comparison only; slow).
    pub fn read_rgba(&self, texture: &wgpu::Texture) -> Vec<Rgba> {
        assert_eq!(texture.format(), wgpu::TextureFormat::Rgba8Unorm, "read_rgba needs an Rgba8Unorm texture");
        let (w, h) = (texture.width(), texture.height());
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            texture.size(),
        );
        self.queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("GPU readback");
        let data = slice.get_mapped_range().expect("map readback range");
        let mut out = Vec::with_capacity((w * h) as usize);
        for y in 0..h as usize {
            let line = &data[y * row as usize..][..w as usize * 4];
            out.extend(line.chunks_exact(4).map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]])));
        }
        out
    }
}

/// How two images differ: identical pixels and the largest per-channel difference among the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Diff {
    pub pixels: usize,
    pub identical: usize,
    pub max_channel: u32,
}

impl Diff {
    pub fn of(a: &[Rgba], b: &[Rgba]) -> Diff {
        assert_eq!(a.len(), b.len());
        let mut d = Diff { pixels: a.len(), identical: 0, max_channel: 0 };
        for (&x, &y) in a.iter().zip(b) {
            if x == y {
                d.identical += 1;
            } else {
                let c = (0..4).map(|i| ((x >> (8 * i)) & 255).abs_diff((y >> (8 * i)) & 255)).max().unwrap();
                d.max_channel = d.max_channel.max(c);
            }
        }
        d
    }

    pub fn percent(&self) -> f64 {
        100.0 * self.identical as f64 / self.pixels.max(1) as f64
    }
}
