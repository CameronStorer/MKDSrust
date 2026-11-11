//! Combined layout review (Widescreen mod): renders `MKSNAP01` snapshots the way the window arranges them for a
//! scene, to PNG. Snapshots and images hold game graphics: keep them local.
//! `arrange_shot <snap.mksnap> <scene> <out.png> [--size WxH] [--widescreen 16:9] [--scale N] [--track]
//!               [--from SCENE --t 0.5]`
//!   scene: a scene id (0 title, 1 select, 2 race, 11 setup, ...), `2f` = race finished (results), or a RAM dump
//!          (`ram_<frame>.bin` from `race_check --dump-frames`) to read it from.
//!   --from/--t: a frame of the transition from SCENE's arrangement at 0..1.
use mkds_native::{arrange, gpu::{pieces, Compositor, Gpu, Renderer3d}, window};
use recomp::machine::RenderSnapshot;

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn parse_phase(s: &str) -> Result<u32, String> {
    if std::path::Path::new(s).is_file() {
        return Ok(arrange::phase(&std::fs::read(s).map_err(|e| format!("{s}: {e}"))?));
    }
    let (id, finished) = s.strip_suffix('f').map_or((s, false), |id| (id, true));
    let id: u32 = id.parse().map_err(|_| format!("bad scene {s}"))?;
    Ok(id | if finished { arrange::FINISHED } else { 0 })
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: arrange_shot <snap.mksnap> <scene> <out.png> [--size WxH] [--widescreen 16:9] [--scale N] [--track] [--from SCENE --t T]".into());
    }
    let snap = RenderSnapshot::decode(&std::fs::read(&args[0]).map_err(|e| format!("{}: {e}", args[0]))?)?;
    let phase = parse_phase(&args[1])?;
    let out_path = &args[2];
    let (mut width, mut height, mut scale, mut track, mut from, mut t) = (1920u32, 1080u32, 4u32, false, None, 1.0f32);
    let mut dump = None::<String>;
    let mut widescreen = mkds_builder::settings::Widescreen::W16x9;
    let mut it = args[3..].iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                (width, height) = (w.parse().map_err(|_| "--size WxH")?, h.parse().map_err(|_| "--size WxH")?);
            }
            "--widescreen" => widescreen = mkds_builder::settings::Widescreen::parse(&value()?).ok_or("--widescreen Off, 16:10, 16:9 or 21:9")?,
            "--scale" => scale = value()?.parse().map_err(|_| "--scale N")?,
            "--track" => track = true,
            "--from" => from = Some(parse_phase(&value()?)?),
            "--t" => t = value()?.parse().map_err(|_| "--t 0..1")?,
            "--dump-backdrop" => dump = Some(value()?),
            _ => return Err(format!("unknown option {a}")),
        }
    }
    let settings = mkds_builder::settings::Settings {
        layout: mkds_builder::settings::Layout::Combined,
        widescreen,
        map_overlay_track: track,
        ..Default::default()
    };
    let display = window::Display {
        settings, controls_path: None, scale: Some(scale), smooth: false, side_by_side: false, fullscreen: false,
        texture_pack: None, dump_textures: None, widescreen_mod: true,
    };
    let margin = display.margin(true);
    let scene = arrange::Scene::of(phase);
    eprintln!("scene {:?} (phase {phase:#x}), margin {margin}, scale {scale}", scene);
    let gpu = Gpu::headless()?;
    let mut renderer = Renderer3d::new_wide(&gpu, scale, margin);
    let mut compositor = Compositor::new_wide(&gpu, scale, margin);
    compositor.set_combined(&gpu, true);
    let race = scene == arrange::Scene::Race;
    compositor.set_overlay(if race { window::map_overlay(&display, margin, true) } else { None });
    let layer = renderer.render(&gpu, &snap);
    compositor.render_raw(&gpu, &snap, layer);
    if let Some(path) = &dump {
        // Both engines' 512x512 backdrop layers side by side (engine A left).
        let back = compositor.backdrop().ok_or("no backdrop")?;
        let read = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: back.size(), usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut e = gpu.device.create_command_encoder(&Default::default());
        e.copy_buffer_to_buffer(back, 0, &read, 0, back.size());
        gpu.queue.submit([e.finish()]);
        read.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| e.to_string())?;
        let data = read.slice(..).get_mapped_range().expect("range");
        let words: &[u32] = bytemuck::cast_slice(&data);
        let mut rgb = Vec::new();
        for y in 0..512 {
            for e in 0..2 {
                for x in 0..512 {
                    let p = words[e * 262144 + y * 512 + x];
                    rgb.extend([(p >> 24) as u8, (p >> 16) as u8, (p >> 8) as u8]);
                }
            }
        }
        write_png(path, 1024, 512, &rgb)?;
    }
    let target = arrange::arrange(scene, width as f32, height as f32, margin);
    let shown = match from {
        Some(p) => arrange::blend(&arrange::arrange(arrange::Scene::of(p), width as f32, height as f32, margin), &target, t),
        None => target,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("arranged"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let pass_tool = pieces::PiecesPass::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
    let out = compositor.output();
    let screens = out.create_view(&Default::default());
    let inputs = pieces::Inputs {
        screens: &screens,
        size: (out.width(), out.height()),
        scale,
        margin,
        backdrop: compositor.backdrop().ok_or("no backdrop")?,
        a_top: snap.a_top(),
    };
    let view = texture.create_view(&Default::default());
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("arranged"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass_tool.draw(&gpu, &mut pass, &inputs, &shown, width, height);
    }
    gpu.queue.submit([encoder.finish()]);
    let pixels = gpu.read_rgba(&texture);
    let rgb: Vec<u8> = pixels.iter().flat_map(|p| [(p >> 24) as u8, (p >> 16) as u8, (p >> 8) as u8]).collect();
    write_png(out_path, width, height, &rgb)?;
    eprintln!("wrote {out_path}");
    Ok(())
}

fn write_png(path: &str, width: u32, height: u32, rgb: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
    let mut png = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    png.set_color(png::ColorType::Rgb);
    png.set_depth(png::BitDepth::Eight);
    let mut writer = png.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())
}
