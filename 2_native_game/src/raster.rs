//! 3D rasterizer, software (T061 / lane L6): turns the polygons the geometry engine ([`crate::gx`])
//! produced for one frame into a 256x192 RGBA image, using the decoded textures ([`crate::texture`]).
//!
//! Implemented: near-plane clipping in clip space, perspective-correct interpolation of vertex colour
//! and texture coordinates, nearest-neighbour texturing with the console's wrap / flip rules, the
//! modulation and decal polygon modes, front / back face selection, polygon alpha blending, and a
//! depth buffer (less-than, or equal-within-tolerance when the polygon asks, with translucent polygons
//! writing depth only when flagged), shadow-volume masks and polygon-ID rejection. Lighting is
//! applied by the geometry engine; toon shading is resolved into vertex colours before rasterizing
//! ([`crate::gx::resolve_toon`]). Not modelled: highlight shading, wireframe (alpha 0) polygons, fog,
//! edge marking, anti-aliasing, per-pixel translucent sort order. Documented console behaviour only.
use crate::gx::Polygon;
use crate::texture::{decode, wrap, TexParams};
use std::collections::HashMap;

pub const W: usize = 256;
pub const H: usize = 192;

/// A clip-space vertex with the attributes interpolated across the polygon.
#[derive(Clone, Copy, Debug)]
struct V {
    x: f64,
    y: f64,
    z: f64,
    w: f64,
    r: f64,
    g: f64,
    b: f64,
    s: f64,
    t: f64,
}

fn lerp(a: &V, b: &V, t: f64) -> V {
    let m = |p: f64, q: f64| p + (q - p) * t;
    V { x: m(a.x, b.x), y: m(a.y, b.y), z: m(a.z, b.z), w: m(a.w, b.w), r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), s: m(a.s, b.s), t: m(a.t, b.t) }
}

/// Sutherland-Hodgman against the near plane (`z >= -w`) and `w > 0`.
fn clip_near(poly: &[V]) -> Vec<V> {
    let inside = |v: &V| v.z + v.w >= 0.0 && v.w > 1e-6;
    let dist = |v: &V| v.z + v.w;
    let mut out = Vec::with_capacity(poly.len() + 1);
    for i in 0..poly.len() {
        let (a, b) = (&poly[i], &poly[(i + 1) % poly.len()]);
        match (inside(a), inside(b)) {
            (true, true) => out.push(*b),
            (true, false) => {
                let t = dist(a) / (dist(a) - dist(b));
                out.push(lerp(a, b, t));
            }
            (false, true) => {
                let t = dist(a) / (dist(a) - dist(b));
                out.push(lerp(a, b, t));
                out.push(*b);
            }
            _ => {}
        }
    }
    out
}

fn expand5(v: u16) -> f64 {
    (v & 31) as f64 / 31.0
}

fn pack(r: f64, g: f64, b: f64, a: f64) -> u32 {
    let q = |v: f64| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
    q(r) << 24 | q(g) << 16 | q(b) << 8 | q(a)
}

pub struct Frame {
    pub color: Vec<u32>,
    pub depth: Vec<f64>,
}

/// The console's polygon list order (GBATEK, SWAP_BUFFERS / polygon sorting): the rendering engine draws
/// every opaque polygon before any translucent one, whatever order the game submitted them in. Translucent
/// polygons are sorted by their screen Y extent (bottom edge, then top edge, top of the screen first) when
/// `swap_param` bit 0 is clear (auto-sort), else they keep their submission order (manual sort). Opaque
/// polygons keep their submission order here: among them the order only decides exact depth ties, and the
/// console's own Y order there is not modelled. The sort is stable. A polygon is translucent when its alpha
/// is 1-30, or when its texture has an alpha channel (A3I5 / A5I3) and it is not a decal or shadow polygon.
///
/// Drawing in submission order instead loses translucent surfaces submitted before the opaque ones behind
/// them (they do not write depth, so the later opaque polygons cover them): Mario Kart DS's course water
/// and waterfalls, and shadow volumes whose masks were built against an incomplete depth buffer.
pub fn hardware_order(polys: &mut [Polygon], viewport: [u8; 4], swap_param: u32) {
    let (y1, y2) = (viewport[1] as f64, viewport[3] as f64);
    let key = |p: &Polygon| -> (bool, u16, u16) {
        let alpha = (p.attr >> 16) & 31;
        let format = (p.tex_param >> 26) & 7;
        let translucent = (1..31).contains(&alpha) || (matches!(format, 1 | 6) && (p.attr >> 4) & 1 == 0);
        if !translucent || swap_param & 1 != 0 {
            return (translucent, 0, 0);
        }
        let verts: Vec<V> = p
            .verts
            .iter()
            .map(|v| V {
                x: v.clip[0] as f64,
                y: v.clip[1] as f64,
                z: v.clip[2] as f64,
                w: v.clip[3] as f64,
                r: 0.0,
                g: 0.0,
                b: 0.0,
                s: 0.0,
                t: 0.0,
            })
            .collect();
        let (mut top, mut bottom) = (u16::MAX, 0u16);
        for v in clip_near(&verts) {
            let y = 191.0 - (y1 + (v.y / v.w + 1.0) * 0.5 * (y2 - y1 + 1.0));
            let y = y.round().clamp(0.0, 191.0) as u16;
            top = top.min(y);
            bottom = bottom.max(y);
        }
        (translucent, bottom, top.min(bottom))
    };
    if polys.len() < 2 {
        return;
    }
    let keys: Vec<_> = polys.iter().map(key).collect();
    let mut order: Vec<usize> = (0..polys.len()).collect();
    order.sort_by_key(|&i| keys[i]);
    if order.iter().enumerate().all(|(n, &i)| n == i) {
        return;
    }
    let sorted: Vec<Polygon> = order.iter().map(|&i| polys[i].clone()).collect();
    polys.clone_from_slice(&sorted);
}

/// Renders `polys` into a frame cleared to `clear` (packed `0xRRGGBBAA`).
pub fn render(polys: &[Polygon], viewport: [u8; 4], tex: &[u8], pal: &[u8], clear: u32) -> Frame {
    let mut f = Frame { color: vec![clear; W * H], depth: vec![f64::INFINITY; W * H] };
    let mut stencil = vec![false; W * H];
    let mut opaque_id = vec![0u8; W * H];
    // 0xff means no translucent fragment has been written since the last opaque one.
    let mut translucent_id = vec![0xffu8; W * H];
    let mut previous_mask = false;
    let mut cache: HashMap<(u32, u32), (TexParams, Vec<u32>)> = HashMap::new();
    let (x1, y1, x2, y2) = (viewport[0] as f64, viewport[1] as f64, viewport[2] as f64, viewport[3] as f64);
    for p in polys {
        let alpha_field = (p.attr >> 16) & 31;
        if alpha_field == 0 || p.verts.len() < 3 {
            continue; // wireframe polygons are not drawn
        }
        let poly_alpha = alpha_field as f64 / 31.0;
        let shadow = (p.attr >> 4) & 3 == 3;
        let id = ((p.attr >> 24) & 63) as u8;
        let mask = shadow && id == 0;
        if mask && !previous_mask {
            stencil.fill(false);
        }
        previous_mask = mask;
        let verts: Vec<V> = p
            .verts
            .iter()
            .map(|v| V {
                x: v.clip[0] as f64,
                y: v.clip[1] as f64,
                z: v.clip[2] as f64,
                w: v.clip[3] as f64,
                r: expand5(v.color),
                g: expand5(v.color >> 5),
                b: expand5(v.color >> 10),
                s: v.st[0] as f64 / 16.0,
                t: v.st[1] as f64 / 16.0,
            })
            .collect();
        let clipped = clip_near(&verts);
        if clipped.len() < 3 {
            continue;
        }
        // screen space: x right, y down; depth 0..1; keep 1/w for perspective correction
        struct S {
            x: f64,
            y: f64,
            z: f64,
            iw: f64,
            r: f64,
            g: f64,
            b: f64,
            s: f64,
            t: f64,
        }
        let scr: Vec<S> = clipped
            .iter()
            .map(|v| {
                let nx = v.x / v.w;
                let ny = v.y / v.w;
                let nz = v.z / v.w;
                let iw = 1.0 / v.w;
                S {
                    x: x1 + (nx + 1.0) * 0.5 * (x2 - x1 + 1.0),
                    y: 191.0 - (y1 + (ny + 1.0) * 0.5 * (y2 - y1 + 1.0)),
                    z: (nz + 1.0) * 0.5,
                    iw,
                    r: v.r * iw,
                    g: v.g * iw,
                    b: v.b * iw,
                    s: v.s * iw,
                    t: v.t * iw,
                }
            })
            .collect();
        // winding in console orientation (y up): positive = counter-clockwise = front face
        let area: f64 = (0..scr.len()).map(|i| {
            let (a, b) = (&scr[i], &scr[(i + 1) % scr.len()]);
            a.x * b.y - b.x * a.y
        }).sum::<f64>()
            * -0.5; // y is flipped, so negate
        let render_back = p.attr & (1 << 6) != 0;
        let render_front = p.attr & (1 << 7) != 0;
        if (area > 0.0 && !render_front) || (area < 0.0 && !render_back) || area == 0.0 {
            continue;
        }
        let tp = TexParams::from_reg(p.tex_param);
        let textured = tp.format != 0;
        let key = (p.tex_param, p.pltt_base);
        if textured && !cache.contains_key(&key) {
            cache.insert(key, (tp, decode(&tp, p.pltt_base, tex, pal)));
        }
        let texels = cache.get(&key).filter(|_| textured);
        let decal = (p.attr >> 4) & 3 == 1 || shadow;
        let write_trans_depth = p.attr & (1 << 11) != 0;
        let depth_equal = p.attr & (1 << 14) != 0;
        for k in 1..scr.len() - 1 {
            let (a, b, c) = (&scr[0], &scr[k], &scr[k + 1]);
            let det = (b.x - a.x) * (c.y - a.y) - (c.x - a.x) * (b.y - a.y);
            if det.abs() < 1e-9 {
                continue;
            }
            let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
            let max_x = (a.x.max(b.x).max(c.x).ceil().min(W as f64 - 1.0)).max(0.0) as usize;
            let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
            let max_y = (a.y.max(b.y).max(c.y).ceil().min(H as f64 - 1.0)).max(0.0) as usize;
            for py in min_y..=max_y {
                for px in min_x..=max_x {
                    let (sx, sy) = (px as f64 + 0.5, py as f64 + 0.5);
                    let l1 = ((sx - a.x) * (c.y - a.y) - (c.x - a.x) * (sy - a.y)) / det;
                    let l2 = ((b.x - a.x) * (sy - a.y) - (sx - a.x) * (b.y - a.y)) / det;
                    let l0 = 1.0 - l1 - l2;
                    if l0 < -1e-9 || l1 < -1e-9 || l2 < -1e-9 {
                        continue;
                    }
                    let z = l0 * a.z + l1 * b.z + l2 * c.z;
                    let idx = py * W + px;
                    let old = f.depth[idx];
                    let pass = if depth_equal { (z - old).abs() <= 0.0001 || z < old } else { z < old };
                    // Shadow ID 0 marks occluded fragments only. It never changes colour/depth.
                    // Nonzero shadow IDs draw inside that mask, excluding their own caster.
                    if mask {
                        stencil[idx] |= !pass;
                        continue;
                    }
                    if shadow && (!stencil[idx] || opaque_id[idx] == id) {
                        continue;
                    }
                    if !pass {
                        continue;
                    }
                    let iw = l0 * a.iw + l1 * b.iw + l2 * c.iw;
                    if iw <= 0.0 {
                        continue;
                    }
                    let interp = |fa: f64, fb: f64, fc: f64| (l0 * fa + l1 * fb + l2 * fc) / iw;
                    let (mut r, mut g, mut bl) = (interp(a.r, b.r, c.r), interp(a.g, b.g, c.g), interp(a.b, b.b, c.b));
                    let mut alpha = poly_alpha;
                    if let Some((tparams, img)) = texels {
                        let s = interp(a.s, b.s, c.s).floor() as i32;
                        let t = interp(a.t, b.t, c.t).floor() as i32;
                        let (tx, ty) = wrap(tparams, s, t);
                        let texel = img.get(ty * tparams.width + tx).copied().unwrap_or(0);
                        let ta = (texel & 0xFF) as f64 / 255.0;
                        if ta == 0.0 {
                            continue; // fully transparent texel: no pixel, no depth write
                        }
                        let (tr, tg, tb) = ((texel >> 24) as f64 / 255.0, ((texel >> 16) & 0xFF) as f64 / 255.0, ((texel >> 8) & 0xFF) as f64 / 255.0);
                        if decal {
                            r = tr * ta + r * (1.0 - ta);
                            g = tg * ta + g * (1.0 - ta);
                            bl = tb * ta + bl * (1.0 - ta);
                        } else {
                            r *= tr;
                            g *= tg;
                            bl *= tb;
                            alpha *= ta;
                        }
                    }
                    if alpha < 1.0 {
                        // Adjacent faces of one translucent model (including a triangulated
                        // quad) must not blend repeatedly at the same pixel.
                        if translucent_id[idx] == id {
                            continue;
                        }
                        translucent_id[idx] = id;
                        let dst = f.color[idx];
                        let da = (dst & 0xFF) as f64 / 255.0;
                        let (dr, dg, db) = ((dst >> 24) as f64 / 255.0, ((dst >> 16) & 0xFF) as f64 / 255.0, ((dst >> 8) & 0xFF) as f64 / 255.0);
                        let out_a = alpha + da * (1.0 - alpha);
                        let mix = |s: f64, d: f64| if da == 0.0 { s } else { s * alpha + d * (1.0 - alpha) };
                        f.color[idx] = pack(mix(r, dr), mix(g, dg), mix(bl, db), out_a);
                        if write_trans_depth {
                            f.depth[idx] = z;
                        }
                    } else {
                        f.color[idx] = pack(r, g, bl, 1.0);
                        f.depth[idx] = z;
                        opaque_id[idx] = id;
                        translucent_id[idx] = 0xff;
                    }
                }
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gx::Vertex;

    fn vert(x: i32, y: i32, z: i32, w: i32, color: u16) -> Vertex {
        Vertex { clip: [x, y, z, w], color, st: [0, 0] }
    }
    const WHITE: u16 = 0x7FFF;
    const RED: u16 = 0x001F;
    const BLUE: u16 = 0x7C00;
    /// Front-face, alpha 31 attributes.
    const ATTR: u32 = (31 << 16) | (1 << 7) | (1 << 6);
    const VP: [u8; 4] = [0, 0, 255, 191];

    fn tri(a: (i32, i32), b: (i32, i32), c: (i32, i32), z: i32, color: u16) -> Polygon {
        Polygon { verts: vec![vert(a.0, a.1, z, 4096, color), vert(b.0, b.1, z, 4096, color), vert(c.0, c.1, z, 4096, color)], attr: ATTR, tex_param: 0, pltt_base: 0 }
    }
    fn px(f: &Frame, x: usize, y: usize) -> u32 {
        f.color[y * W + x]
    }

    #[test]
    fn a_triangle_covers_its_area_and_not_outside() {
        // big counter-clockwise triangle around the screen centre
        let p = tri((-2048, -2048), (2048, -2048), (0, 2048), 0, RED);
        let f = render(&[p], VP, &[], &[], 0);
        assert_eq!(px(&f, 128, 96), 0xFF0000FF, "red, opaque");
        assert_eq!(px(&f, 2, 2), 0, "outside stays clear");
        assert_eq!(px(&f, 128, 5), 0, "above the apex");
    }

    #[test]
    fn nearer_polygon_wins_regardless_of_order() {
        let far = tri((-3000, -3000), (3000, -3000), (0, 3000), 3000, BLUE);
        let near = tri((-3000, -3000), (3000, -3000), (0, 3000), -3000, RED);
        for order in [[far.clone(), near.clone()], [near.clone(), far.clone()]] {
            let f = render(&order, VP, &[], &[], 0);
            assert_eq!(px(&f, 128, 96), 0xFF0000FF);
        }
    }

    #[test]
    fn back_faces_are_culled_unless_requested() {
        let mut p = tri((-2048, -2048), (0, 2048), (2048, -2048), 0, RED); // clockwise
        p.attr = (31 << 16) | (1 << 7); // front only
        assert_eq!(px(&render(&[p.clone()], VP, &[], &[], 0), 128, 96), 0, "back face culled");
        p.attr = (31 << 16) | (1 << 6); // back only
        assert_eq!(px(&render(&[p], VP, &[], &[], 0), 128, 96), 0xFF0000FF);
    }

    #[test]
    fn colours_interpolate_perspective_correctly() {
        // a horizontal red->blue gradient on a flat triangle (w constant): the middle is a 50/50 mix
        let mut p = tri((-4000, -2000), (4000, -2000), (0, 3000), 0, WHITE);
        p.verts[0].color = RED;
        p.verts[1].color = BLUE;
        p.verts[2].color = RED | BLUE;
        let f = render(&[p], VP, &[], &[], 0);
        let c = px(&f, 128, 120);
        let (r, b) = ((c >> 24) as i32, ((c >> 8) & 0xFF) as i32);
        assert!((r - b).abs() < 40, "centre is a mix: r {r} b {b}");
        let left = px(&f, 50, 138);
        assert!((left >> 24) > ((left >> 8) & 0xFF) + 40, "left side redder: {left:#010X}");
        let right = px(&f, 205, 138);
        assert!(((right >> 8) & 0xFF) > (right >> 24) + 40, "right side bluer: {right:#010X}");
    }

    #[test]
    fn translucent_polygons_blend_and_skip_depth_by_default() {
        let back = tri((-3000, -3000), (3000, -3000), (0, 3000), 2000, BLUE);
        let mut glass = tri((-3000, -3000), (3000, -3000), (0, 3000), 0, RED);
        glass.attr = (16 << 16) | (1 << 7); // alpha 16/31, no depth write
        let f = render(&[back, glass], VP, &[], &[], 0);
        let c = px(&f, 128, 96);
        assert!((c >> 24) > 60 && ((c >> 8) & 0xFF) > 60, "red and blue both present: {c:#010X}");
        assert!(f.depth[96 * W + 128] > 0.7, "translucent polygon did not write depth");
    }

    #[test]
    fn translucent_polygons_submitted_first_still_show_over_opaque_ones_behind() {
        // a translucent surface submitted before the opaque ground behind it (course water)
        let mut water = tri((-3000, -3000), (3000, -3000), (0, 3000), 0, RED);
        water.attr = (16 << 16) | (1 << 7);
        let ground = tri((-3000, -3000), (3000, -3000), (0, 3000), 2000, BLUE);
        let submitted = render(&[water.clone(), ground.clone()], VP, &[], &[], 0);
        assert_eq!(px(&submitted, 128, 96) >> 24, 0, "submission order: the ground covers the water");
        let mut polys = vec![water, ground];
        hardware_order(&mut polys, VP, 0);
        assert_eq!(polys[0].attr, ATTR, "opaque first");
        let ordered = render(&polys, VP, &[], &[], 0);
        let c = px(&ordered, 128, 96);
        assert!((c >> 24) > 60 && ((c >> 8) & 0xFF) > 60, "water blended over the ground: {c:#010X}");
    }

    #[test]
    fn opaque_polygons_keep_their_order_and_translucent_ones_sort_by_y_unless_manual() {
        let low = tri((-3000, -3000), (3000, -3000), (0, -2000), 0, RED); // bottom of the screen
        let high = tri((-3000, 2000), (3000, 2000), (0, 3000), 0, BLUE); // top of the screen
        let mut glass_a = low.clone();
        glass_a.attr = (16 << 16) | (1 << 7);
        let mut glass_b = high.clone();
        glass_b.attr = (16 << 16) | (1 << 7);
        let mut polys = vec![glass_a.clone(), low.clone(), glass_b.clone(), high.clone()];
        hardware_order(&mut polys, VP, 1);
        assert_eq!(polys, vec![low.clone(), high.clone(), glass_a.clone(), glass_b.clone()]);
        let mut polys = vec![glass_a.clone(), low.clone(), glass_b.clone(), high.clone()];
        hardware_order(&mut polys, VP, 0);
        assert_eq!(polys, vec![low, high, glass_b, glass_a], "auto-sort: translucent by Y");
    }

    fn shadow(z: i32, id: u32) -> Polygon {
        let mut p = tri((-3000, -3000), (3000, -3000), (0, 3000), z, 0);
        p.attr = (ATTR & !(31 << 16)) | (13 << 16) | (3 << 4) | (id << 24);
        p
    }

    #[test]
    fn shadow_mask_never_writes_colour_or_depth() {
        let receiver = tri((-3000, -3000), (3000, -3000), (0, 3000), 0, WHITE);
        for z in [-1000, 1000] {
            let f = render(&[receiver.clone(), shadow(z, 0)], VP, &[], &[], 0);
            assert_eq!(px(&f, 128, 96), 0xffffffff);
            assert_eq!(f.depth[96 * W + 128], 0.5);
        }
    }

    #[test]
    fn visible_shadow_requires_failed_mask_depth_and_different_caster_id() {
        for masked in [false, true] {
            for caster in [false, true] {
                let mut receiver = tri((-3000, -3000), (3000, -3000), (0, 3000), 0, WHITE);
                receiver.attr |= (if caster { 3 } else { 8 }) << 24;
                let mask = shadow(if masked { 1000 } else { -1000 }, 0);
                let f = render(&[receiver, mask, shadow(-1000, 3)], VP, &[], &[], 0);
                if masked && !caster {
                    let r = px(&f, 128, 96) >> 24;
                    assert!((145..=150).contains(&r), "one alpha-13 shadow: {r}");
                } else {
                    assert_eq!(px(&f, 128, 96), 0xffffffff);
                }
                assert_eq!(f.depth[96 * W + 128], 0.5);
            }
        }
        assert_eq!(px(&render(&[shadow(-1000, 3)], VP, &[], &[], 0), 128, 96), 0);
    }

    #[test]
    fn new_shadow_mask_group_does_not_reuse_the_previous_volume() {
        let receiver = tri((-3000, -3000), (3000, -3000), (0, 3000), 0, WHITE);
        let first = vec![receiver, shadow(1000, 0), shadow(-1000, 3)];
        let expected = render(&first, VP, &[], &[], 0).color;
        let mut second = first;
        second.extend([shadow(-1000, 0), shadow(-1000, 4)]);
        assert_eq!(render(&second, VP, &[], &[], 0).color, expected);
    }

    #[test]
    fn same_translucent_id_blends_once_but_distinct_ids_can_overlap() {
        let back = tri((-3000, -3000), (3000, -3000), (0, 3000), 2000, WHITE);
        let mut glass = shadow(0, 3);
        glass.attr &= !(3 << 4); // ordinary translucent polygon
        let once = render(&[back.clone(), glass.clone()], VP, &[], &[], 0).color;
        assert_eq!(render(&[back.clone(), glass.clone(), glass.clone()], VP, &[], &[], 0).color, once);
        let mut other = glass.clone();
        other.attr ^= (3 ^ 4) << 24;
        let twice = render(&[back, glass, other], VP, &[], &[], 0);
        assert!(px(&twice, 128, 96) >> 24 < once[96 * W + 128] >> 24);
    }

    #[test]
    fn direct_colour_texture_is_sampled_and_modulated() {
        // 8x8 direct-colour texture (format 7): left half red, right half blue, opaque (bit 15)
        let mut tex = vec![0u8; 8 * 8 * 2];
        for y in 0..8 {
            for x in 0..8 {
                let c: u16 = 0x8000 | if x < 4 { RED } else { BLUE };
                let o = (y * 8 + x) * 2;
                tex[o..o + 2].copy_from_slice(&c.to_le_bytes());
            }
        }
        let mut quad = Polygon {
            verts: vec![vert(-4000, -3000, 0, 4096, WHITE), vert(4000, -3000, 0, 4096, WHITE), vert(4000, 3000, 0, 4096, WHITE), vert(-4000, 3000, 0, 4096, WHITE)],
            attr: ATTR,
            tex_param: 7 << 26, // direct colour, 8x8, offset 0, no repeat
            pltt_base: 0,
        };
        quad.verts[0].st = [0, 0];
        quad.verts[1].st = [8 * 16, 0];
        quad.verts[2].st = [8 * 16, 8 * 16];
        quad.verts[3].st = [0, 8 * 16];
        let f = render(&[quad], VP, &tex, &[], 0);
        let l = px(&f, 70, 96);
        let r = px(&f, 190, 96);
        assert_eq!(l >> 24, 255, "left is red: {l:#010X}");
        assert_eq!((r >> 8) & 0xFF, 255, "right is blue: {r:#010X}");
    }

    #[test]
    fn polygons_crossing_the_eye_plane_are_clipped_not_dropped() {
        // one vertex behind the camera (w < 0): the visible part must still be drawn
        let p = Polygon {
            verts: vec![vert(-2000, -2000, 1000, 4096, RED), vert(2000, -2000, 1000, 4096, RED), vert(0, 8000, -9000, -4096, RED)],
            attr: ATTR,
            tex_param: 0,
            pltt_base: 0,
        };
        let f = render(&[p], VP, &[], &[], 0);
        assert!(f.color.iter().any(|&c| c == 0xFF0000FF), "something visible survives clipping");
    }
}
