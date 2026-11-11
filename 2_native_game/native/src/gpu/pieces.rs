//! Combined layout (Widescreen mod): draws [`crate::arrange::Piece`]s, parts of the composed screens and the
//! menus' wrapping backdrop layer, into the window with soft edges and alpha blending.
use super::Gpu;
use crate::arrange::{Piece, Source};

const MAX: usize = 16;

const WGSL: &str = r#"
struct Piece { dst: vec4<f32>, src: vec4<f32>, wrap: vec4<f32>, fade: vec4<f32>, params: vec4<f32>, color: vec4<f32> };
struct Globals { size: vec4<f32>, pieces: array<Piece, 16> };
@group(0) @binding(0) var screens: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<storage, read> back: array<u32>;
@group(0) @binding(3) var<uniform> g: Globals;
struct Out { @builtin(position) pos: vec4<f32>, @location(0) t: vec2<f32>, @location(1) @interpolate(flat) piece: u32 };
@vertex fn vs(@builtin(vertex_index) i: u32, @builtin(instance_index) n: u32) -> Out {
    let corner = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0))[i];
    let d = g.pieces[n].dst;
    let p = d.xy + corner * d.zw;
    var o: Out;
    o.pos = vec4<f32>(p.x / g.size.x * 2.0 - 1.0, 1.0 - p.y / g.size.y * 2.0, 0.0, 1.0);
    o.t = corner;
    o.piece = n;
    return o;
}
fn unpack(c: u32) -> vec3<f32> { return vec3<f32>(f32((c >> 24u) & 255u), f32((c >> 16u) & 255u), f32((c >> 8u) & 255u)) / 255.0; }
fn back_at(engine: u32, p: vec2<i32>) -> vec3<f32> {
    let x = u32(p.x & 511); let y = u32(p.y & 511);
    return unpack(back[engine * 262144u + y * 512u + x]);
}
fn to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}
@fragment fn fs(o: Out) -> @location(0) vec4<f32> {
    let p = g.pieces[o.piece];
    let kind = u32(p.params.y);
    var c = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    let s = p.src.xy + o.t * p.src.zw;
    if kind == 0u {
        var u = s.x;
        if p.wrap.y > p.wrap.x { u = p.wrap.x + fract((u - p.wrap.x) / (p.wrap.y - p.wrap.x)) * (p.wrap.y - p.wrap.x); }
        c = textureSampleLevel(screens, linear_sampler, vec2<f32>(u, s.y), 0.0);
        if p.params.z == 0.0 { c.a = 1.0; }
    } else if kind == 1u {
        // Sharp bilinear: crisp DS pixels without uneven widths at fractional scales.
        let engine = u32(p.params.w);
        let q = s - 0.5;
        let i = vec2<i32>(floor(q));
        var f = fract(q);
        f = clamp((f - 0.5) * p.wrap.z + 0.5, vec2<f32>(0.0), vec2<f32>(1.0));
        let top = mix(back_at(engine, i), back_at(engine, i + vec2(1, 0)), f.x);
        let bottom = mix(back_at(engine, i + vec2(0, 1)), back_at(engine, i + vec2(1, 1)), f.x);
        var rgb = mix(top, bottom, f.y);
        if g.size.z != 0.0 { rgb = to_linear(rgb); }
        c = vec4<f32>(rgb, 1.0);
    } else {
        var rgb = p.color.rgb;
        if g.size.z != 0.0 { rgb = to_linear(rgb); }
        c = vec4<f32>(rgb, 1.0);
    }
    // Soft edges.
    let w = o.pos.xy;
    let d = p.dst;
    var a = c.a * p.params.x;
    if p.fade.x > 0.0 { a *= smoothstep(0.0, p.fade.x, w.x - d.x); }
    if p.fade.y > 0.0 { a *= smoothstep(0.0, p.fade.y, d.x + d.z - w.x); }
    if p.fade.z > 0.0 { a *= smoothstep(0.0, p.fade.z, w.y - d.y); }
    if p.fade.w > 0.0 { a *= smoothstep(0.0, p.fade.w, d.y + d.w - w.y); }
    return vec4<f32>(c.rgb * a, a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Raw {
    dst: [f32; 4],
    src: [f32; 4],
    wrap: [f32; 4],
    fade: [f32; 4],
    params: [f32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    size: [f32; 4],
    pieces: [Raw; MAX],
}

/// What the pieces sample this frame.
pub struct Inputs<'a> {
    /// The compositor output (both screens, top first), viewed as the window's encoding.
    pub screens: &'a wgpu::TextureView,
    /// Its size in texels, the compositor's scale and widescreen margin.
    pub size: (u32, u32),
    pub scale: u32,
    pub margin: u32,
    /// Both engines' backdrop layers (`Compositor::backdrop`).
    pub backdrop: &'a wgpu::Buffer,
    /// Engine A shows on the top screen.
    pub a_top: bool,
}

pub struct PiecesPass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    srgb: bool,
}

impl PiecesPass {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> PiecesPass {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pieces"), source: wgpu::ShaderSource::Wgsl(WGSL.into()) });
        let entry = |binding, visibility, ty| wgpu::BindGroupLayoutEntry { binding, visibility, ty, count: None };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pieces"),
            entries: &[
                entry(0, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }),
                entry(1, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
                entry(2, wgpu::ShaderStages::FRAGMENT, wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }),
                entry(3, wgpu::ShaderStages::VERTEX_FRAGMENT, wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pieces"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pieces"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pieces"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        PiecesPass { pipeline, layout, sampler, uniform, srgb: format.is_srgb() }
    }

    /// Draws `pieces` (back to front) into `pass`, a render pass over a `width` x `height` target.
    pub fn draw(&self, gpu: &Gpu, pass: &mut wgpu::RenderPass<'_>, inputs: &Inputs, pieces: &[Piece], width: u32, height: u32) {
        let mut globals = Globals { size: [width as f32, height as f32, self.srgb as u32 as f32, 0.0], pieces: [bytemuck::Zeroable::zeroed(); MAX] };
        let (tw, th) = (inputs.size.0 as f32, inputs.size.1 as f32);
        let s = inputs.scale as f32;
        let mut n = 0;
        for p in pieces.iter().filter(|p| p.alpha > 0.001 && p.dst[2] >= 1.0 && p.dst[3] >= 1.0).take(MAX) {
            let [x, y, w, h] = p.src;
            let (kind, src, wrap, engine, color) = match p.source {
                Source::Screen { bottom } => {
                    // DS pixels of a screen -> texels of the compositor output (screens stacked, margin columns).
                    let row = if bottom { 192.0 } else { 0.0 };
                    let ux = |v: f32| (v + inputs.margin as f32) * s / tw;
                    let src = [ux(x), (y + row) * s / th, w * s / tw, h * s / th];
                    let wrap = p.wrap.map_or([0.0; 4], |[a, b]| [ux(a), ux(b), 0.0, 0.0]);
                    (0.0, src, wrap, 0.0, [0.0; 4])
                }
                Source::Backdrop { bottom } => {
                    let engine = if bottom == inputs.a_top { 1.0 } else { 0.0 };
                    let sharp = (p.dst[2] / w.max(1.0)).max(1.0);
                    (1.0, [x, y, w, h], [0.0, 0.0, sharp, 0.0], engine, [0.0; 4])
                }
                Source::Solid(c) => {
                    let ch = |sh: u32| ((c >> sh) & 255) as f32 / 255.0;
                    (2.0, [0.0; 4], [0.0; 4], 0.0, [ch(16), ch(8), ch(0), 1.0])
                }
            };
            globals.pieces[n] = Raw { dst: p.dst, src, wrap, fade: p.fade, params: [p.alpha, kind, p.see_through as u32 as f32, engine], color };
            n += 1;
        }
        if n == 0 {
            return;
        }
        gpu.queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&globals));
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pieces"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(inputs.screens) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: inputs.backdrop.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: self.uniform.as_entire_binding() },
            ],
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..6, 0..n as u32);
    }
}
