struct Vertex { pos: vec4<f32>, color: vec4<f32>, uv: vec4<f32> }
struct Triangle { edges: array<vec4<f32>, 3>, a: Vertex, b: Vertex, c: Vertex, flags: vec4<u32> }
struct Frame { clear: vec4<f32>, scale: u32, tiles_x: u32, pad1: u32, pad2: u32 }
@group(0) @binding(0) var<storage, read> triangles: array<Triangle>;
@group(0) @binding(1) var<storage, read> bins: array<u32>;
@group(0) @binding(2) var texels: texture_2d_array<u32>;
@group(0) @binding(3) var output: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(4) var<uniform> frame: Frame;
fn unpack(v: u32) -> vec4<f32> {
    return vec4<f32>(f32(v >> 24u), f32((v >> 16u) & 255u), f32((v >> 8u) & 255u), f32(v & 255u)) / 255.0;
}
fn wrap(v: i32, size: i32, repeat: bool, flip: bool) -> u32 {
    if !repeat { return u32(clamp(v, 0, size - 1)); }
    let period = size * select(1, 2, flip);
    let m = ((v % period) + period) % period;
    return u32(select(m, 2 * size - 1 - m, flip && m >= size));
}
// Wrap the integer coordinate in original space, then mirror/clamp the
// fractional position too. This keeps high-resolution detail continuous at seams.
fn replacement_axis(v: i32, wrapped: u32, sub: u32, size: u32, factor: u32, repeat: bool, flip: bool) -> u32 {
    if !repeat {
        if v < 0 { return 0u; }
        if v >= i32(size) { return size * factor - 1u; }
    }
    var part = min(sub, factor - 1u);
    let period = i32(size * 2u);
    let m = ((v % period) + period) % period;
    if repeat && flip && m >= i32(size) { part = factor - 1u - part; }
    return wrapped * factor + part;
}
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let pixel = (vec2<f32>(gid.xy) + 0.5) / f32(frame.scale);
    let tile = (gid.y / frame.scale / 8u) * frame.tiles_x + gid.x / frame.scale / 8u;
    var color = frame.clear;
    var depth = 3.402823e38;
    var opaque_id = 0u;
    var translucent_id = 255u;
    var stencil = false;
    var epoch = 0u;
    for (var index = bins[tile]; index < bins[tile + 1u]; index++) {
        let tri = triangles[bins[index]];
        let attr = tri.flags.x;
        let shadow = ((attr >> 4u) & 3u) == 3u;
        let id = (attr >> 24u) & 63u;
        if tri.flags.w != epoch { epoch = tri.flags.w; stencil = false; }
        let basis = vec3<f32>(1.0, pixel);
        if min(dot(tri.edges[0].xyz, basis), min(dot(tri.edges[1].xyz, basis), dot(tri.edges[2].xyz, basis))) < -1e-9 { continue; }
        let z = tri.a.pos.z + pixel.x * tri.b.pos.z + pixel.y * tri.c.pos.z;
        let depth_pass = z < depth || ((attr & (1u << 14u)) != 0u && abs(z - depth) <= 0.0001);
        if shadow && id == 0u { stencil = stencil || !depth_pass; continue; }
        if !depth_pass || (shadow && (!stencil || opaque_id == id)) { continue; }
        let iw = tri.a.pos.w + pixel.x * tri.b.pos.w + pixel.y * tri.c.pos.w;
        if iw <= 0.0 { continue; }
        var rgb = (tri.a.color.xyz + pixel.x * tri.b.color.xyz + pixel.y * tri.c.color.xyz) / iw;
        var alpha = f32((attr >> 16u) & 31u) / 31.0;
        let reg = tri.flags.y;
        if ((reg >> 26u) & 7u) != 0u {
            let coord = (tri.a.uv.xy + pixel.x * tri.b.uv.xy + pixel.y * tri.c.uv.xy) / iw;
            let uv = vec2<i32>(floor(coord));
            let width = 8u << ((reg >> 20u) & 7u); let height = 8u << ((reg >> 23u) & 7u);
            let x = wrap(uv.x, i32(width), (reg & (1u << 16u)) != 0u, (reg & (1u << 18u)) != 0u);
            let y = wrap(uv.y, i32(height), (reg & (1u << 17u)) != 0u, (reg & (1u << 19u)) != 0u);
            let factor = u32(tri.a.uv.z);
            var at = tri.flags.z + y * width + x;
            if factor > 1u {
                let sub = vec2<u32>(floor(fract(coord) * f32(factor)));
                let sx = replacement_axis(uv.x, x, sub.x, width, factor, (reg & (1u << 16u)) != 0u, (reg & (1u << 18u)) != 0u);
                let sy = replacement_axis(uv.y, y, sub.y, height, factor, (reg & (1u << 17u)) != 0u, (reg & (1u << 19u)) != 0u);
                at = tri.flags.z + sy * width * factor + sx;
            }
            let t = unpack(textureLoad(texels, vec2<i32>(i32(at & 1023u), i32((at >> 10u) & 1023u)), i32(at >> 20u), 0).r);
            if t.a == 0.0 { continue; }
            if ((attr >> 4u) & 3u) == 1u || shadow { rgb = t.rgb * t.a + rgb * (1.0 - t.a); }
            else { rgb *= t.rgb; alpha *= t.a; }
        }
        if alpha < 1.0 {
            if translucent_id == id { continue; }
            translucent_id = id;
            if color.a != 0.0 { rgb = rgb * alpha + color.rgb * (1.0 - alpha); }
            color = floor(clamp(vec4<f32>(rgb, alpha + color.a * (1.0 - alpha)), vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0 + 0.5) / 255.0;
            if (attr & (1u << 11u)) != 0u { depth = z; }
        } else {
            color = floor(clamp(vec4<f32>(rgb, 1.0), vec4<f32>(0.0), vec4<f32>(1.0)) * 255.0 + 0.5) / 255.0;
            depth = z; opaque_id = id; translucent_id = 255u;
        }
    }
    textureStore(output, vec2<i32>(gid.xy), color);
}


