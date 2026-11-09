// Integer translation of Engine2d's scan/fold. Zero colour denotes a transparent sample.
@group(0) @binding(0) var<storage, read> raw: array<u32>;
@group(0) @binding(1) var<storage, read_write> memory: array<u32>;
@group(0) @binding(2) var<uniform> maps: array<vec4<u32>, 9>;
@group(0) @binding(3) var<storage, read_write> base: array<u32>;
@group(0) @binding(4) var<storage, read_write> other: array<u32>;
@group(0) @binding(5) var<storage, read_write> info: array<u32>;
// Map overlay (Widescreen mod): the top layer's own colour, 0 where it is the backdrop or the background layer
// (the enabled background drawn furthest back), so a map can be shown without its backdrop pattern.
@group(0) @binding(6) var<storage, read_write> top: array<u32>;
const STRIDE = 0xca000u;
var<private> engine: u32;
var<private> d: u32;

@compute @workgroup_size(256)
fn map_memory(@builtin(global_invocation_id) id: vec3<u32>) {
    let a = id.x * 4u;
    if a >= STRIDE * 2u { return; }
    var value = 0u;
    for (var b = 0u; b < 9u; b++) {
        let m = maps[b];
        if a >= m.x && a - m.x < m.y { value |= raw[(a - m.x + m.z) / 4u]; }
    }
    memory[id.x] = value;
}
fn byte(a: u32) -> u32 { return (raw[a / 4u] >> ((a & 3u) * 8u)) & 255u; }
fn half(a: u32) -> u32 { return byte(a) | (byte(a + 1u) << 8u); }
fn reg(a: u32) -> u32 { return half(engine * 0x1000u + a); }
fn oam(a: u32) -> u32 { return half(0x1900u + engine * 0x400u + a); }
fn pal(a: u32) -> u32 { return half(0x1100u + engine * 0x400u + a * 2u); }
fn mem(a: u32) -> u32 {
    return (memory[(engine * STRIDE + a) / 4u] >> ((a & 3u) * 8u)) & 255u;
}
fn vram(a: u32) -> u32 {
    let bg = 0x06000000u + engine * 0x200000u;
    let obj = 0x06400000u + engine * 0x200000u;
    if a >= bg && a - bg < select(0x80000u, 0x20000u, engine != 0u) { return mem(a - bg); }
    if a >= obj && a - obj < select(0x40000u, 0x20000u, engine != 0u) { return mem(0x80000u + a - obj); }
    return 0u;
}
fn vr16(a: u32) -> u32 { return vram(a) | (vram(a + 1u) << 8u); }
fn ext(obj: bool, index: u32) -> u32 {
    let a = select(0xc0000u, 0xc8000u, obj) + index * 2u;
    return mem(a) | (mem(a + 1u) << 8u);
}
fn rgb(c: u32) -> u32 {
    let v = vec3<u32>(c, c >> 5u, c >> 10u) & vec3<u32>(31u);
    let e = (v << vec3<u32>(3u)) | (v >> vec3<u32>(2u));
    return (e.x << 24u) | (e.y << 16u) | (e.z << 8u) | 255u;
}
fn signed(v: u32) -> i32 { return i32(v << 16u) >> 16u; }
fn text_pixel(bg: u32, x: u32, y: u32) -> u32 {
    let cnt = reg(8u + 2u * bg);
    let start = 0x06000000u + engine * 0x200000u;
    let chars = start + ((d >> 24u) & 7u) * 0x10000u + ((cnt >> 2u) & 15u) * 0x4000u;
    let screen = start + ((d >> 27u) & 7u) * 0x10000u + ((cnt >> 8u) & 31u) * 0x800u;
    let size = (cnt >> 14u) & 3u;
    let px = (x + (reg(0x10u + bg * 4u) & 511u)) & select(255u, 511u, (size & 1u) != 0u);
    let py = (y + (reg(0x12u + bg * 4u) & 511u)) & select(255u, 511u, (size & 2u) != 0u);
    var block = 0u;
    switch size { case 1u: { block = px / 256u; } case 2u: { block = py / 256u; }
        case 3u: { block = py / 256u * 2u + px / 256u; } default: {} }
    let ent = vr16(screen + block * 0x800u + ((py % 256u) / 8u * 32u + (px % 256u) / 8u) * 2u);
    let ix = select(px % 8u, 7u - px % 8u, (ent & 0x400u) != 0u);
    let iy = select(py % 8u, 7u - py % 8u, (ent & 0x800u) != 0u);
    let tile = ent & 1023u;
    if (cnt & 0x80u) != 0u {
        let index = vram(chars + tile * 64u + iy * 8u + ix);
        if index == 0u { return 0u; }
        if (d & (1u << 30u)) != 0u {
            let slot = select(bg, bg + 2u, bg < 2u && (cnt & 0x2000u) != 0u);
            return rgb(ext(false, slot * 4096u + (ent >> 12u) * 256u + index));
        }
        return rgb(pal(index));
    }
    let b = vram(chars + tile * 32u + iy * 4u + ix / 2u);
    let index = select(b & 15u, b >> 4u, (ix & 1u) != 0u);
    if index == 0u { return 0u; }
    return rgb(pal((ent >> 12u) * 16u + index));
}
fn affine_pixel(bg: u32, x: u32, y: u32, extended: bool) -> u32 {
    let cnt = reg(8u + bg * 2u);
    let r = 0x20u + (bg - 2u) * 0x10u;
    let refx = i32((reg(r + 8u) | (reg(r + 10u) << 16u)) << 4u) >> 4u;
    let refy = i32((reg(r + 12u) | (reg(r + 14u) << 16u)) << 4u) >> 4u;
    var sx = (refx + signed(reg(r)) * i32(x) + signed(reg(r + 2u)) * i32(y)) >> 8u;
    var sy = (refy + signed(reg(r + 4u)) * i32(x) + signed(reg(r + 6u)) * i32(y)) >> 8u;
    let size = (cnt >> 14u) & 3u;
    let bitmap = extended && (cnt & 0x80u) != 0u;
    var width = 128i << size;
    var height = width;
    if bitmap { width = array<i32,4>(128,256,512,512)[size]; height = array<i32,4>(128,256,256,512)[size]; }
    if (cnt & 0x2000u) != 0u { sx = sx & (width - 1); sy = sy & (height - 1); }
    else if sx < 0 || sy < 0 || sx >= width || sy >= height { return 0u; }
    let start = 0x06000000u + engine * 0x200000u;
    if bitmap {
        let address = start + ((cnt >> 8u) & 31u) * 0x4000u;
        let offset = u32(sy * width + sx);
        if (cnt & 4u) != 0u {
            let c = vr16(address + offset * 2u);
            return select(0u, rgb(c), (c & 0x8000u) != 0u);
        }
        let index = vram(address + offset);
        return select(0u, rgb(pal(index)), index != 0u);
    }
    let address = start + ((d >> 27u) & 7u) * 0x10000u + ((cnt >> 8u) & 31u) * 0x800u;
    let map_index = u32(sy / 8 * (width / 8) + sx / 8);
    var ent = vram(address + map_index);
    if extended { ent = vr16(address + map_index * 2u); }
    let tile = select(ent, ent & 1023u, extended);
    let ix = select(u32(sx) % 8u, 7u - u32(sx) % 8u, extended && (ent & 0x400u) != 0u);
    let iy = select(u32(sy) % 8u, 7u - u32(sy) % 8u, extended && (ent & 0x800u) != 0u);
    let chars = start + ((d >> 24u) & 7u) * 0x10000u + ((cnt >> 2u) & 15u) * 0x4000u;
    let index = vram(chars + tile * 64u + iy * 8u + ix);
    if index == 0u { return 0u; }
    if extended && (d & (1u << 30u)) != 0u { return rgb(ext(false, bg * 4096u + (ent >> 12u) * 256u + index)); }
    return rgb(pal(index));
}
fn background(bg: u32, x: u32, y: u32) -> u32 {
    let mode = d & 7u;
    if mode == 0u || (mode == 1u && (bg < 2u || bg == 3u)) || (mode >= 2u && mode <= 5u && bg < 2u) || (mode == 3u && bg == 2u) { return text_pixel(bg,x,y); }
    if ((mode == 1u || mode == 2u || mode == 4u) && bg == 2u) || (mode == 2u && bg == 3u) { return affine_pixel(bg,x,y,false); }
    if (mode >= 3u && mode <= 5u && bg == 3u) || (mode == 5u && bg == 2u) { return affine_pixel(bg,x,y,true); }
    return 0u;
}
// priority, colour, semitransparent; transparent colour also marks no sprite.
fn sprite(x: u32, y: u32, window: bool) -> vec3<u32> {
    var found = vec3<u32>(4u,0u,0u);
    for (var i = 0u; i < 128u; i++) {
        let a0 = oam(i * 8u); let a1 = oam(i * 8u + 2u); let a2 = oam(i * 8u + 4u);
        let affine = (a0 & 0x100u) != 0u;
        if !affine && (a0 & 0x200u) != 0u { continue; }
        let mode = (a0 >> 10u) & 3u;
        if (mode == 2u) != window { continue; }
        let shape = (a0 >> 14u) & 3u;
        if shape == 3u { continue; }
        let size = (a1 >> 14u) & 3u;
        let dims = array<vec2<u32>,12>(vec2(8u,8u),vec2(16u,16u),vec2(32u,32u),vec2(64u,64u),
            vec2(16u,8u),vec2(32u,8u),vec2(32u,16u),vec2(64u,32u),
            vec2(8u,16u),vec2(8u,32u),vec2(16u,32u),vec2(32u,64u))[shape * 4u + size];
        let w = dims.x; let h = dims.y;
        let bounds = dims * select(1u,2u,affine && (a0 & 0x200u) != 0u);
        let rel = (y + 256u - (a0 & 255u)) & 255u;
        let ox = i32((a1 & 511u) << 23u) >> 23u;
        let sx = i32(x) - ox;
        let priority = (a2 >> 10u) & 3u;
        if rel >= bounds.y || sx < 0 || sx >= i32(bounds.x) || found.x <= priority { continue; }
        var rx = u32(sx); var ry = rel;
        if affine {
            let matrix = ((a1 >> 9u) & 31u) * 32u;
            let dx = sx - i32(bounds.x / 2u); let dy = i32(rel) - i32(bounds.y / 2u);
            let ax = ((signed(oam(matrix + 6u)) * dx + signed(oam(matrix + 14u)) * dy) >> 8u) + i32(w / 2u);
            let ay = ((signed(oam(matrix + 22u)) * dx + signed(oam(matrix + 30u)) * dy) >> 8u) + i32(h / 2u);
            if ax < 0 || ay < 0 || ax >= i32(w) || ay >= i32(h) { continue; }
            rx = u32(ax); ry = u32(ay);
        } else {
            rx = select(rx,w - 1u - rx,(a1 & 0x1000u) != 0u);
            ry = select(ry,h - 1u - ry,(a1 & 0x2000u) != 0u);
        }
        let tile = a2 & 1023u; let c256 = (a0 & 0x2000u) != 0u;
        let bpt = select(32u,64u,c256);
        let start = 0x06400000u + engine * 0x200000u;
        var address = start + (tile + rx / 8u * (bpt / 32u) + ry / 8u * 32u) * 32u;
        if (d & 16u) != 0u { address = start + tile * (32u << ((d >> 20u) & 3u)) + (ry / 8u * (w / 8u) + rx / 8u) * bpt; }
        let ix = rx % 8u; let iy = ry % 8u;
        var color = 0u;
        if mode == 3u {
            if (a2 >> 12u) == 0u { continue; }
            if (d & 0x40u) != 0u { address = start + tile * (128u << ((d >> 22u) & 1u)) + (ry * w + rx) * 2u; }
            else {
                let width = select(128u,256u,(d & 0x20u) != 0u); let tiles = width / 8u;
                address = start + (tile % tiles * 8u + tile / tiles * 8u * width + ry * width + rx) * 2u;
            }
            let c = vr16(address);
            color = select(0u,rgb(c),(c & 0x8000u) != 0u);
        } else if c256 {
            let index = vram(address + iy * 8u + ix);
            if index != 0u {
                if (d & (1u << 31u)) != 0u { color = rgb(ext(true,(a2 >> 12u) * 256u + index)); }
                else { color = rgb(pal(256u + index)); }
            }
        } else {
            let b = vram(address + iy * 4u + ix / 2u);
            let index = select(b & 15u,b >> 4u,(ix & 1u) != 0u);
            if index != 0u { color = rgb(pal(256u + (a2 >> 12u) * 16u + index)); }
        }
        if color != 0u { found = vec3(priority,color,u32(mode == 1u)); }
    }
    return found;
}
fn contains(range: u32, point: u32) -> bool {
    let start = range >> 8u; let end = range & 255u;
    if start <= end { return point >= start && point < end; }
    return point >= start || point < end;
}
fn window_mask(x: u32, y: u32, object: bool) -> u32 {
    if (d & 0xe000u) == 0u { return 63u; }
    for (var i = 0u; i < 2u; i++) {
        if (d & (0x2000u << i)) != 0u && contains(reg(0x40u + i * 2u),x) && contains(reg(0x44u + i * 2u),y) { return (reg(0x48u) >> (i * 8u)) & 63u; }
    }
    return (reg(0x4au) >> select(0u,8u,object)) & 63u;
}
fn brighten(c: u32, mode: u32, amount: u32) -> u32 {
    var v = vec3<u32>(c >> 24u,c >> 16u,c >> 8u) & vec3<u32>(255u);
    if mode == 2u { v += (vec3<u32>(255u) - v) * amount / 16u; } else { v -= v * amount / 16u; }
    return (v.x << 24u) | (v.y << 16u) | (v.z << 8u) | 255u;
}
fn blend_stack(mask: u32, a: vec4<u32>, b: vec4<u32>) -> u32 {
    let blend = reg(0x50u); let mode = (blend >> 6u) & 3u;
    var best = a.y | 255u;
    if (mask & 32u) != 0u {
        let selected = (blend & (1u << a.x)) != 0u;
        if (a.z != 0u || (mode == 1u && selected)) && (blend & (0x100u << b.x)) != 0u {
            var ea = min(reg(0x52u) & 31u,16u); var eb = min((reg(0x52u) >> 8u) & 31u,16u); var divisor = 16u;
            if a.x == 0u && (d & 8u) != 0u { ea = a.y & 255u; eb = 255u - ea; divisor = 255u; }
            let av = vec3<u32>(a.y >> 24u,a.y >> 16u,a.y >> 8u) & vec3<u32>(255u);
            let bv = vec3<u32>(b.y >> 24u,b.y >> 16u,b.y >> 8u) & vec3<u32>(255u);
            let v = min((av * ea + bv * eb) / divisor,vec3<u32>(255u));
            best = (v.x << 24u) | (v.y << 16u) | (v.z << 8u) | 255u;
        } else if selected && (mode == 2u || mode == 3u) { best = brighten(a.y,mode,min(reg(0x54u) & 31u,16u)); }
    }
    return best;
}
fn fold(mask: u32, scanned: array<vec4<u32>,3>, n: u32, with3: bool) -> array<vec4<u32>,2> {
    let blend = reg(0x50u);
    let backdrop = vec4<u32>(5u,rgb(pal(0u)),0u,0u);
    var layers = array<vec4<u32>,2>(backdrop,backdrop);
    var count = 0u;
    for (var i = 0u; i < n; i++) {
        var a = scanned[i];
        if a.w != 0u { if !with3 { continue; } a.y = 0x80u; }
        layers[count] = a; count++;
        let alpha = (a.z != 0u && (a.x != 0u || (a.y & 255u) != 255u)) || ((blend >> 6u) & 3u) == 1u && (blend & (1u << a.x)) != 0u;
        if count == 2u || (mask & 32u) == 0u || (blend & 0x3f00u) == 0u || !alpha { break; }
    }
    return layers;
}
@compute @workgroup_size(8,8)
fn split(@builtin(global_invocation_id) id: vec3<u32>) {
    engine = id.z;
    d = reg(0u) | (reg(2u) << 16u);
    let x = id.x; let y = id.y; let index = engine * 49152u + y * 256u + x;
    base[index] = 0xffffffffu; other[index] = 0u; info[index] = 0u; top[index] = 0u;
    let display = (d >> 16u) & 3u;
    if display == 0u || (d & 0x80u) != 0u { return; }
    if display != 1u { base[index] = 0u; return; }
    var sp = vec3<u32>(4u,0u,0u); var object = false;
    if (d & 0x1000u) != 0u { sp = sprite(x,y,false); }
    if (d & 0x9000u) == 0x9000u { object = sprite(x,y,true).y != 0u; }
    let mask = window_mask(x,y,object);
    var scanned: array<vec4<u32>,3>; var n = 0u;
    for (var priority = 0u; priority < 4u; priority++) {
        if sp.y != 0u && (mask & 16u) != 0u && sp.x == priority { scanned[n] = vec4<u32>(4u,sp.y,sp.z,0u); n++; }
        if n == 3u { break; }
        for (var bg = 0u; bg < 4u; bg++) {
            if (mask & (1u << bg)) == 0u { continue; }
            if (d & (0x100u << bg)) == 0u || (reg(8u + bg * 2u) & 3u) != priority { continue; }
            if bg == 0u && (d & 8u) != 0u { scanned[n] = vec4<u32>(0u,0u,1u,1u); }
            else {
                let color = background(bg,x,y);
                if color == 0u { continue; }
                scanned[n] = vec4<u32>(bg,color,0u,0u);
            }
            n++; if n == 3u { break; }
        }
        if n == 3u { break; }
    }
    let without = fold(mask,scanned,n,false);
    base[index] = blend_stack(mask,without[0],without[1]);
    var back = 5u; var back_key = 0u;
    for (var bg = 0u; bg < 4u; bg++) {
        if (d & (0x100u << bg)) == 0u || (bg == 0u && (d & 8u) != 0u) { continue; }
        let key = ((reg(8u + bg * 2u) & 3u) << 2u) | bg;
        if back == 5u || key >= back_key { back = bg; back_key = key; }
    }
    if without[0].x != 5u && without[0].x != back { top[index] = without[0].y | 255u; }
    if (d & 0x108u) != 0x108u || (mask & 1u) == 0u { return; }
    let stack3 = fold(mask,scanned,n,true);
    let effects = (mask & 32u) != 0u; let blend = reg(0x50u); let mode = (blend >> 6u) & 3u;
    if stack3[0].x == 0u {
        info[index] = 1u | (u32(effects && (blend & (0x100u << stack3[1].x)) != 0u) << 2u)
            | (u32(effects && (blend & 0x2000u) != 0u) << 3u)
            | (u32(effects && (blend & 1u) != 0u && (mode == 2u || mode == 3u)) << 4u);
        other[index] = stack3[1].y;
    } else if stack3[1].x == 0u {
        let a = stack3[0];
        if effects && (a.z != 0u || (mode == 1u && (blend & (1u << a.x)) != 0u)) && (blend & 0x100u) != 0u {
            info[index] = 2u; other[index] = a.y;
        } else { info[index] = 3u; other[index] = blend_stack(mask,a,vec4<u32>(0u,0u,1u,0u)); }
    }
}

