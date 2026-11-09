// Screen compositor (block 2 stage B2.3): `recomp::gfx2d::Split::pixel` for every sample of both screens.
// Integer maths throughout, so a 3D layer of 256x192 reproduces the software screens exactly.
struct Engine { eva: u32, evb: u32, bright_mode: u32, bright: u32, master_mode: u32, master: u32, pad0: u32, pad1: u32 }
struct Frame { engines: array<Engine, 2>, scale: u32, a_top: u32, margin: u32, pad1: u32 }
@group(0) @binding(0) var<storage, read> base: array<u32>;   // [engine][pixel] packed RRGGBBAA
@group(0) @binding(1) var<storage, read> other: array<u32>;  // [engine][pixel]
@group(0) @binding(2) var<storage, read> info: array<u32>;   // [engine][pixel]
@group(0) @binding(3) var layer3d: texture_2d<f32>;          // (256+2*margin)*scale x 192*scale, engine A only
@group(0) @binding(4) var output: texture_storage_2d<rgba8unorm, write>; // (256+2*margin)*scale x 384*scale
@group(0) @binding(5) var<uniform> frame: Frame;

fn ch(c: u32, s: u32) -> u32 { return (c >> s) & 255u; }
fn pack(r: u32, g: u32, b: u32) -> u32 { return (r << 24u) | (g << 16u) | (b << 8u) | 255u; }
fn brighten(c: u32, mode: u32, amount: u32) -> u32 {
    var o = vec3<u32>(ch(c, 24u), ch(c, 16u), ch(c, 8u));
    if mode == 2u { o = o + (vec3<u32>(255u) - o) * amount / 16u; } else { o = o - o * amount / 16u; }
    return pack(o.x, o.y, o.z);
}
fn mix_weights(a: u32, wa: u32, b: u32, wb: u32, div: u32) -> u32 {
    let r = min((ch(a, 24u) * wa + ch(b, 24u) * wb) / div, 255u);
    let g = min((ch(a, 16u) * wa + ch(b, 16u) * wb) / div, 255u);
    let bl = min((ch(a, 8u) * wa + ch(b, 8u) * wb) / div, 255u);
    return pack(r, g, bl);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let s = frame.scale;
    let m = frame.margin * s;
    if gid.x >= 256u * s + 2u * m || gid.y >= 384u * s { return; }
    let bottom = gid.y >= 192u * s;
    let y3 = gid.y - select(0u, 192u * s, bottom);
    // Engine A is on top when a_top; the other screen shows engine B.
    let engine = select(select(1u, 0u, frame.a_top != 0u), select(0u, 1u, frame.a_top != 0u), bottom);
    let e = frame.engines[engine];
    var c3 = 0u;
    if engine == 0u {
        let t = vec4<u32>(round(textureLoad(layer3d, vec2<u32>(gid.x, y3), 0) * 255.0));
        c3 = (t.x << 24u) | (t.y << 16u) | (t.z << 8u) | t.w;
    }
    let a3 = c3 & 255u;
    // Widescreen side columns: no 2D layers exist there. The 3D screen shows its 3D layer (black where empty),
    // the other screen is black; the master brightness (fades) still applies.
    if gid.x < m || gid.x >= m + 256u * s {
        var side = select(pack(0u, 0u, 0u), c3 | 255u, a3 != 0u);
        if (e.master_mode == 1u || e.master_mode == 2u) && e.master != 0u {
            side = brighten(side, select(3u, 2u, e.master_mode == 1u), e.master);
        }
        textureStore(output, vec2<u32>(gid.xy), vec4<f32>(vec4<u32>(ch(side, 24u), ch(side, 16u), ch(side, 8u), 255u)) / 255.0);
        return;
    }
    let i = engine * 49152u + (y3 / s) * 256u + (gid.x - m) / s;
    let kind = info[i] & 3u;
    let o = other[i];
    var c = base[i];
    if a3 != 0u && kind != 0u {
        if kind == 1u {
            let blends = select((info[i] & 4u) != 0u, (info[i] & 8u) != 0u, a3 == 255u);
            if blends { c = mix_weights(c3, a3, o, 255u - a3, 255u); }
            else if (info[i] & 16u) != 0u { c = brighten(c3, e.bright_mode, e.bright); }
            else { c = c3 | 255u; }
        } else if kind == 2u {
            c = mix_weights(o, e.eva, c3, e.evb, 16u);
        } else {
            c = o;
        }
    }
    if (e.master_mode == 1u || e.master_mode == 2u) && e.master != 0u {
        c = brighten(c, select(3u, 2u, e.master_mode == 1u), e.master);
    }
    textureStore(output, vec2<u32>(gid.xy), vec4<f32>(vec4<u32>(ch(c, 24u), ch(c, 16u), ch(c, 8u), 255u)) / 255.0);
}
