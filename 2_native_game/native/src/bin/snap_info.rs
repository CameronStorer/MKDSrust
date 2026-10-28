//! Private local renderer diagnostic: prints a render snapshot's display registers and its 3D polygon list.
//! Snapshots hold game graphics; keep them and anything written here out of the repository.
//! `snap_info <file.mksnap> [--polys] [--render OUT.ppm [--only A-B] [--skip A-B]]`
//!   --polys   one line per polygon: index, attributes, texture format, depth (z/w) and w ranges
//!   --order P apply the console polygon order (`raster::hardware_order`, SWAP_BUFFERS parameter P) first
//!   --pixel X,Y  list the polygons (in drawing order) whose projection covers that top-screen 3D pixel
//!   --render  software screens as PPM; --only / --skip keep or drop a polygon index range (inclusive)
use recomp::machine::RenderSnapshot;

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

fn range(v: Option<String>) -> Result<(usize, usize), String> {
    let v = v.ok_or("range needs A-B")?;
    let (a, b) = v.split_once('-').ok_or("range needs A-B")?;
    Ok((a.parse().map_err(|_| "bad range")?, b.parse().map_err(|_| "bad range")?))
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: snap_info <file.mksnap> [--polys] [--render OUT.ppm [--only A-B] [--skip A-B]]")?;
    let (mut polys, mut render, mut only, mut skip, mut order, mut pixel) = (false, None, None, None, None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--polys" => polys = true,
            "--pixel" => pixel = Some(range(args.next().map(|v| v.replace(',', "-")))?),
            "--order" => order = Some(args.next().and_then(|v| v.parse::<u32>().ok()).ok_or("--order needs a number")?),
            "--render" => render = Some(args.next().ok_or("--render needs a file")?),
            "--only" => only = Some(range(args.next())?),
            "--skip" => skip = Some(range(args.next())?),
            _ => return Err(format!("unknown option {a}")),
        }
    }
    let mut snap = RenderSnapshot::decode(&std::fs::read(&path).map_err(|e| e.to_string())?)?;
    if let Some(param) = order {
        recomp::raster::hardware_order(&mut snap.polys, snap.viewport, param);
    }
    let io = &snap.io;
    let r32 = |o: usize| u32::from_le_bytes([io[o], io[o + 1], io[o + 2], io[o + 3]]);
    let r16 = |o: usize| u16::from_le_bytes([io[o], io[o + 1]]);
    println!(
        "DISPCNT_A {:08x} DISPCNT_B {:08x} DISPCAPCNT {:08x} POWCNT1 {:04x} DISP3DCNT {:04x} VRAMCNT {:02x?} CLEAR {:08x} viewport {:?} polys {}",
        r32(0),
        r32(0x1000),
        r32(0x64),
        r16(0x304),
        r16(0x60),
        &io[0x240..0x24a],
        r32(0x350),
        snap.viewport,
        snap.polys.len()
    );
    if polys {
        for (i, p) in snap.polys.iter().enumerate() {
            let a = p.attr;
            let zw = |v: &recomp::gx::Vertex| v.clip[2] as f64 / v.clip[3] as f64;
            let (zmin, zmax) = p.verts.iter().map(zw).fold((f64::MAX, f64::MIN), |(l, h), z| (l.min(z), h.max(z)));
            let (wmin, wmax) = p.verts.iter().map(|v| v.clip[3]).fold((i32::MAX, i32::MIN), |(l, h), w| (l.min(w), h.max(w)));
            println!(
                "{i:5} n{} light{:x} mode{} back{} front{} xlu_depth{} far{} dot{} deq{} fog{} alpha{:2} id{:2} | fmt{} {}x{} rep{}{} flip{}{} col0{} gen{} addr{:05x} pal{:04x} | z/w {zmin:.4}..{zmax:.4} w {wmin}..{wmax}",
                p.verts.len(),
                a & 15,
                (a >> 4) & 3,
                (a >> 6) & 1,
                (a >> 7) & 1,
                (a >> 11) & 1,
                (a >> 12) & 1,
                (a >> 13) & 1,
                (a >> 14) & 1,
                (a >> 15) & 1,
                (a >> 16) & 31,
                (a >> 24) & 63,
                (p.tex_param >> 26) & 7,
                8 << ((p.tex_param >> 20) & 7),
                8 << ((p.tex_param >> 23) & 7),
                (p.tex_param >> 16) & 1,
                (p.tex_param >> 17) & 1,
                (p.tex_param >> 18) & 1,
                (p.tex_param >> 19) & 1,
                (p.tex_param >> 29) & 1,
                p.tex_param >> 30,
                (p.tex_param & 0xffff) << 3,
                p.pltt_base,
            );
        }
    }
    if let Some((px, py)) = pixel {
        let (x1, y1, x2, y2) = (snap.viewport[0] as f64, snap.viewport[1] as f64, snap.viewport[2] as f64, snap.viewport[3] as f64);
        let (sx, sy) = (px as f64 + 0.5, py as f64 + 0.5);
        for (i, p) in snap.polys.iter().enumerate() {
            if p.verts.iter().any(|v| v.clip[3] <= 0) {
                continue; // crossing the eye plane: not handled by this quick test
            }
            let s: Vec<(f64, f64)> = p
                .verts
                .iter()
                .map(|v| {
                    let (w, x, y) = (v.clip[3] as f64, v.clip[0] as f64, v.clip[1] as f64);
                    (x1 + (x / w + 1.0) * 0.5 * (x2 - x1 + 1.0), 191.0 - (y1 + (y / w + 1.0) * 0.5 * (y2 - y1 + 1.0)))
                })
                .collect();
            let n = s.len();
            let cross: Vec<f64> = (0..n)
                .map(|k| {
                    let (a, b) = (s[k], s[(k + 1) % n]);
                    (b.0 - a.0) * (sy - a.1) - (b.1 - a.1) * (sx - a.0)
                })
                .collect();
            if cross.iter().all(|&c| c >= 0.0) || cross.iter().all(|&c| c <= 0.0) {
                let a = p.attr;
                println!("{i:5} mode{} alpha{:2} id{:2} xlu_depth{} fmt{} z/w {:.5}", (a >> 4) & 3, (a >> 16) & 31, (a >> 24) & 63, (a >> 11) & 1, (p.tex_param >> 26) & 7,
                    p.verts.iter().map(|v| v.clip[2] as f64 / v.clip[3] as f64).sum::<f64>() / n as f64);
            }
        }
    }
    if let Some(out) = render {
        let keep = |i: usize| {
            only.is_none_or(|(a, b)| (a..=b).contains(&i)) && skip.is_none_or(|(a, b)| !(a..=b).contains(&i))
        };
        snap.polys = snap.polys.iter().enumerate().filter(|(i, _)| keep(*i)).map(|(_, p)| p.clone()).collect();
        let px = snap.screens();
        let mut o = b"P6\n256 384\n255\n".to_vec();
        for p in px {
            o.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
        std::fs::write(&out, o).map_err(|e| e.to_string())?;
    }
    Ok(())
}
