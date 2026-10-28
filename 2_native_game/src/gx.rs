//! GX geometry engine, high level (T061 / lane L6): turns the stream of geometry commands the game
//! writes to the GX FIFO / command ports (`ds_mem::Bus::gx`, offsets 0x400..0x6FF from the I/O base)
//! into renderer-neutral polygons: vertices already transformed to clip space (1.19.12 fixed point),
//! with colour, texture coordinate and the polygon / texture attributes the game set. Matrix stacks,
//! vertex packing and primitive assembly follow the console's documented behaviour. No game data.
//! Not modelled yet: clipping,
//! viewport, box/pos/vec tests (their results are ignored) and the render-time depth/alpha rules.
use std::collections::VecDeque;

pub type Mat = [[i32; 4]; 4];

pub const IDENTITY: Mat = [
    [4096, 0, 0, 0],
    [0, 4096, 0, 0],
    [0, 0, 4096, 0],
    [0, 0, 0, 4096],
];

fn mul(a: &Mat, b: &Mat) -> Mat {
    let mut r = [[0; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            let mut s = 0i64;
            for k in 0..4 {
                s += a[i][k] as i64 * b[k][j] as i64;
            }
            r[i][j] = (s >> 12) as i32;
        }
    }
    r
}

/// `v` (row vector, w included) times `m`.
fn xform(v: [i32; 4], m: &Mat) -> [i32; 4] {
    let mut r = [0; 4];
    for j in 0..4 {
        let mut s = 0i64;
        for i in 0..4 {
            s += v[i] as i64 * m[i][j] as i64;
        }
        r[j] = (s >> 12) as i32;
    }
    r
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    /// Clip-space x, y, z, w (1.19.12).
    pub clip: [i32; 4],
    /// RGB555 as set by COLOR.
    pub color: u16,
    /// Texture coordinate (1.11.4), as set by TEXCOORD (not transformed).
    pub st: [i16; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Polygon {
    pub verts: Vec<Vertex>,
    pub attr: u32,
    pub tex_param: u32,
    pub pltt_base: u32,
}

/// Toon shading (DISP3DCNT bit 1 clear): in a toon/highlight polygon (mode 2) the vertex colour's red (0-31)
/// indexes the toon table (0x04000380) and that colour replaces the vertex colour; the polygon then modulates
/// like mode 0, which it becomes here so the table is never applied twice. Mario Kart DS uses it for the star's
/// rainbow and the flash after a hit. Highlight shading (bit 1 set: table colour added after modulation) is not
/// modelled and stays as it was.
pub fn resolve_toon(polys: &mut [Polygon], io: &[u8]) {
    let half = |o: usize| u16::from_le_bytes([io[o], io[o + 1]]);
    if half(0x60) & 2 != 0 {
        return;
    }
    for p in polys.iter_mut().filter(|p| (p.attr >> 4) & 3 == 2) {
        for v in &mut p.verts {
            v.color = half(0x380 + 2 * (v.color & 31) as usize) & 0x7fff;
        }
        p.attr &= !(3 << 4);
    }
}

/// Number of parameter words after each geometry command (None: not a command).
pub fn param_count(cmd: u8) -> Option<usize> {
    Some(match cmd {
        0x00 => 0,
        0x10 => 1,
        0x11 => 0,
        0x12 => 1,
        0x13 | 0x14 => 1,
        0x15 => 0,
        0x16 => 16,
        0x17 => 12,
        0x18 => 16,
        0x19 => 12,
        0x1A => 9,
        0x1B | 0x1C => 3,
        0x20..=0x22 => 1,
        0x23 => 2,
        0x24..=0x2B => 1,
        0x30..=0x33 => 1,
        0x34 => 32,
        0x40 => 1,
        0x41 => 0,
        0x50 => 1,
        0x60 => 1,
        0x70 => 3,
        0x71 => 2,
        0x72 => 1,
        _ => return None,
    })
}

#[derive(Default)]
pub struct Gx {
    mode: u32,
    proj: Mat,
    proj_stack: Mat,
    pos: Mat,
    vecm: Mat,
    tex: Mat,
    pos_stack: Vec<Mat>,
    vec_stack: Vec<Mat>,
    sp: usize,
    color: u16,
    st: [i16; 2],
    raw_st: [i16; 2],
    normal: [i32; 3],
    vtx: [i32; 3],
    attr: u32,
    tex_param: u32,
    pltt_base: u32,
    prim: Option<u32>,
    verts: Vec<Vertex>,
    strip_odd: bool,
    /// Finished polygons, in submission order. The renderer drains this on SWAP_BUFFERS.
    pub polygons: Vec<Polygon>,
    /// Number of SWAP_BUFFERS commands seen (frames).
    pub swaps: u32,
    /// The last SWAP_BUFFERS parameter: bit 0 translucent sort (0 = auto by Y, 1 = manual), bit 1 depth (W).
    pub swap_param: u32,
    /// Commands whose number is not a geometry command (diagnostics).
    pub unknown: Vec<u8>,
    // FIFO unpacking
    queue: VecDeque<u8>,
    params: Vec<u32>,
    need: usize,
    cur: u8,
    /// VIEWPORT: x1, y1, x2, y2 (screen pixels, y up from the bottom as the console defines it).
    pub viewport: [u8; 4],
    pub lights: [[u32; 2]; 4],
    pub material: [u32; 4],
    light_dirs: [[i32; 3]; 4],
    shininess: Vec<u8>,
}

impl Gx {
    /// Read-only matrix result ports, in the same row-vector order as command data.
    pub fn matrix_result(&self, offset: u32) -> Option<u32> {
        if (0x640..0x680).contains(&offset) {
            let index = ((offset - 0x640) / 4) as usize;
            let clip = mul(&self.pos, &self.proj);
            Some(clip[index / 4][index % 4] as u32)
        } else if (0x680..0x6a4).contains(&offset) {
            let index = ((offset - 0x680) / 4) as usize;
            Some(self.vecm[index / 3][index % 3] as u32)
        } else {
            None
        }
    }

    pub fn new() -> Gx {
        let mut g = Gx {
            proj: IDENTITY,
            proj_stack: IDENTITY,
            pos: IDENTITY,
            vecm: IDENTITY,
            tex: IDENTITY,
            shininess: vec![0; 128],
            ..Default::default()
        };
        g.pos_stack = vec![IDENTITY; 32];
        g.vec_stack = vec![IDENTITY; 32];
        g
    }

    /// Feed one write seen by the bus: `off` is the I/O offset (0x400 = FIFO, 0x440.. command ports).
    pub fn write(&mut self, off: u32, value: u32) {
        match off {
            0x400..=0x43F => self.fifo_word(value),
            0x440..=0x5FF => {
                let cmd = ((off - 0x400) / 4) as u8;
                if let Some(n) = param_count(cmd) {
                    // direct port: one command; a command with several parameters takes repeated writes
                    if self.need == 0 || self.cur != cmd {
                        self.cur = cmd;
                        self.need = n;
                        self.params.clear();
                    }
                    if n == 0 {
                        self.exec(cmd, &[]);
                        self.need = 0;
                        return;
                    }
                    self.params.push(value);
                    if self.params.len() == n {
                        let p = std::mem::take(&mut self.params);
                        self.need = 0;
                        self.exec(cmd, &p);
                    }
                }
            }
            _ => {}
        }
    }

    /// Replay a bus log (`ds_mem::Bus::gx`: I/O offset, value, width); returns how many entries it consumed.
    pub fn feed_log(&mut self, log: &[(u32, u32, u8)]) -> usize {
        for &(off, v, _) in log {
            self.write(off, v);
        }
        log.len()
    }

    fn fifo_word(&mut self, w: u32) {
        if self.need > 0 {
            self.params.push(w);
            if self.params.len() == self.need {
                let p = std::mem::take(&mut self.params);
                self.need = 0;
                let c = self.cur;
                self.exec(c, &p);
                self.advance();
            }
            return;
        }
        if self.queue.is_empty() {
            // a packed word of up to four command bytes
            for i in 0..4 {
                let c = (w >> (8 * i)) as u8;
                if c != 0 {
                    self.queue.push_back(c);
                }
            }
            self.advance();
        }
    }

    /// Start the next queued command; run those without parameters at once.
    fn advance(&mut self) {
        while self.need == 0 {
            let Some(c) = self.queue.pop_front() else {
                return;
            };
            match param_count(c) {
                None => self.unknown.push(c),
                Some(0) => self.exec(c, &[]),
                Some(n) => {
                    self.cur = c;
                    self.need = n;
                    self.params.clear();
                }
            }
        }
    }

    fn each_target(&mut self, f: impl Fn(&mut Mat)) {
        match self.mode {
            0 => f(&mut self.proj),
            1 => f(&mut self.pos),
            2 => {
                f(&mut self.pos);
                f(&mut self.vecm);
            }
            _ => f(&mut self.tex),
        }
    }

    fn exec(&mut self, c: u8, p: &[u32]) {
        let s = |i: usize| p[i] as i32;
        match c {
            0x10 => self.mode = p[0] & 3,
            0x11 => match self.mode {
                0 => self.proj_stack = self.proj,
                1 | 2 => {
                    let i = self.sp & 31;
                    self.pos_stack[i] = self.pos;
                    self.vec_stack[i] = self.vecm;
                    self.sp = (self.sp + 1).min(32);
                }
                _ => {}
            },
            0x12 => match self.mode {
                0 => self.proj = self.proj_stack,
                1 | 2 => {
                    let n = ((p[0] & 0x3F) as i32) << 26 >> 26;
                    self.sp = (self.sp as i32 - n).clamp(0, 31) as usize;
                    self.pos = self.pos_stack[self.sp];
                    self.vecm = self.vec_stack[self.sp];
                }
                _ => {}
            },
            0x13 => {
                let i = (p[0] & 31) as usize;
                self.pos_stack[i] = self.pos;
                self.vec_stack[i] = self.vecm;
            }
            0x14 => {
                let i = (p[0] & 31) as usize;
                self.pos = self.pos_stack[i];
                self.vecm = self.vec_stack[i];
            }
            0x15 => self.each_target(|m| *m = IDENTITY),
            0x16 => {
                let m = mat4(p);
                self.each_target(|t| *t = m);
            }
            0x17 => {
                let m = mat43(p);
                self.each_target(|t| *t = m);
            }
            0x18 => {
                let m = mat4(p);
                self.each_target(|t| *t = mul(&m, t));
            }
            0x19 => {
                let m = mat43(p);
                self.each_target(|t| *t = mul(&m, t));
            }
            0x1A => {
                let m = [
                    [s(0), s(1), s(2), 0],
                    [s(3), s(4), s(5), 0],
                    [s(6), s(7), s(8), 0],
                    [0, 0, 0, 4096],
                ];
                self.each_target(|t| *t = mul(&m, t));
            }
            0x1B => {
                let m = [
                    [s(0), 0, 0, 0],
                    [0, s(1), 0, 0],
                    [0, 0, s(2), 0],
                    [0, 0, 0, 4096],
                ];
                // Scale affects positions but never the direction matrix.
                match self.mode {
                    0 => self.proj = mul(&m, &self.proj),
                    3 => self.tex = mul(&m, &self.tex),
                    _ => self.pos = mul(&m, &self.pos),
                }
            }
            0x1C => {
                let m = [
                    [4096, 0, 0, 0],
                    [0, 4096, 0, 0],
                    [0, 0, 4096, 0],
                    [s(0), s(1), s(2), 4096],
                ];
                self.each_target(|t| *t = mul(&m, t));
            }
            0x20 => self.color = (p[0] & 0x7FFF) as u16,
            0x21 => {
                self.normal = [
                    sext(p[0], 10) << 3,
                    sext(p[0] >> 10, 10) << 3,
                    sext(p[0] >> 20, 10) << 3,
                ];
                self.light_vertex();
                if self.tex_param >> 30 == 2 {
                    self.texture_from_vector(self.normal);
                }
            }
            0x22 => {
                self.raw_st = [p[0] as i16, (p[0] >> 16) as i16];
                match self.tex_param >> 30 {
                    0 => self.st = self.raw_st,
                    1 => {
                        for c in 0..2 {
                            self.st[c] = ((self.raw_st[0] as i64 * self.tex[0][c] as i64
                                + self.raw_st[1] as i64 * self.tex[1][c] as i64
                                + self.tex[2][c] as i64
                                + self.tex[3][c] as i64)
                                >> 12) as i16;
                        }
                    }
                    _ => {}
                }
            }
            0x23 => {
                self.vtx = [
                    p[0] as i16 as i32,
                    (p[0] >> 16) as i16 as i32,
                    p[1] as i16 as i32,
                ];
                self.vertex();
            }
            0x24 => {
                self.vtx = [
                    sext(p[0], 10) << 6,
                    sext(p[0] >> 10, 10) << 6,
                    sext(p[0] >> 20, 10) << 6,
                ];
                self.vertex();
            }
            0x25 => {
                self.vtx[0] = p[0] as i16 as i32;
                self.vtx[1] = (p[0] >> 16) as i16 as i32;
                self.vertex();
            }
            0x26 => {
                self.vtx[0] = p[0] as i16 as i32;
                self.vtx[2] = (p[0] >> 16) as i16 as i32;
                self.vertex();
            }
            0x27 => {
                self.vtx[1] = p[0] as i16 as i32;
                self.vtx[2] = (p[0] >> 16) as i16 as i32;
                self.vertex();
            }
            0x28 => {
                // VTX_DIFF is signed 10-bit with twelve fractional bits,
                // unlike NORMAL's nine fractional bits. Coordinates already
                // use fx12; shifting these deltas exaggerates geometry 8x.
                self.vtx[0] += sext(p[0], 10);
                self.vtx[1] += sext(p[0] >> 10, 10);
                self.vtx[2] += sext(p[0] >> 20, 10);
                self.vertex();
            }
            0x29 => self.attr = p[0],
            0x2A => self.tex_param = p[0],
            0x2B => self.pltt_base = p[0] & 0x1FFF,
            0x30 => {
                self.material[0] = p[0];
                if p[0] & 0x8000 != 0 {
                    self.color = (p[0] & 0x7fff) as u16;
                }
            }
            0x31 => self.material[1] = p[0],
            0x32 => {
                let i = (p[0] >> 30) as usize;
                self.lights[i][0] = p[0];
                let v = xform(
                    [
                        sext(p[0], 10) << 3,
                        sext(p[0] >> 10, 10) << 3,
                        sext(p[0] >> 20, 10) << 3,
                        0,
                    ],
                    &self.vecm,
                );
                self.light_dirs[i].copy_from_slice(&v[..3]);
            }
            0x33 => self.lights[(p[0] >> 30) as usize][1] = p[0] & 0x7fff,
            0x34 => self.shininess = p.iter().flat_map(|v| v.to_le_bytes()).collect(),
            0x40 => {
                self.prim = Some(p[0] & 3);
                self.verts.clear();
                self.strip_odd = false;
            }
            0x41 => self.prim = None,
            0x60 => {
                self.viewport = [
                    p[0] as u8,
                    (p[0] >> 8) as u8,
                    (p[0] >> 16) as u8,
                    (p[0] >> 24) as u8,
                ]
            }
            0x50 => {
                self.swap_param = p[0] & 3;
                self.swaps += 1;
                self.prim = None;
            }
            _ => {}
        }
    }

    /// Clip-space vertex -> screen (x, y in pixels with y down from the top of a 192-line screen, depth 0..1).
    /// Returns None for w <= 0 (behind the eye; clipping is not modelled).
    pub fn screen(&self, v: &Vertex) -> Option<(f32, f32, f32)> {
        let w = v.clip[3];
        if w <= 0 {
            return None;
        }
        let (x1, y1, x2, y2) = (
            self.viewport[0] as f32,
            self.viewport[1] as f32,
            self.viewport[2] as f32,
            self.viewport[3] as f32,
        );
        let nx = v.clip[0] as f32 / w as f32;
        let ny = v.clip[1] as f32 / w as f32;
        let nz = v.clip[2] as f32 / w as f32;
        let sx = x1 + (nx + 1.0) * 0.5 * (x2 - x1 + 1.0);
        let sy_up = y1 + (ny + 1.0) * 0.5 * (y2 - y1 + 1.0);
        Some((sx, 191.0 - sy_up, (nz + 1.0) * 0.5))
    }

    /// NORMAL computes vertex colour using the material and enabled directional
    /// lights. Light vectors are latched in eye space when LIGHT_VECTOR is sent.
    fn light_vertex(&mut self) {
        let n = xform(
            [self.normal[0], self.normal[1], self.normal[2], 0],
            &self.vecm,
        );
        let mut color = [0i64; 3];
        for (c, out) in color.iter_mut().enumerate() {
            *out = ((self.material[1] >> (16 + c * 5)) & 31) as i64;
        }
        for i in 0..4 {
            if self.attr & (1 << i) == 0 {
                continue;
            }
            let l = self.light_dirs[i];
            let dot = |v: [i32; 3]| -(0..3).map(|j| n[j] as i64 * v[j] as i64).sum::<i64>() / 4096;
            let diffuse = dot(l).clamp(0, 4096);
            let half = [l[0] / 2, l[1] / 2, (l[2] - 4096) / 2];
            let h = dot(half).clamp(0, 4096);
            let mut specular = h * h / 4096;
            if self.material[1] & 0x8000 != 0 {
                specular = self
                    .shininess
                    .get((specular * 128 / 4096).min(127) as usize)
                    .copied()
                    .unwrap_or(0) as i64
                    * 4096
                    / 256;
            }
            for (c, out) in color.iter_mut().enumerate() {
                let channel = |v: u32| ((v >> (c * 5)) & 31) as i64;
                let material = channel(self.material[0] >> 16)
                    + channel(self.material[0]) * diffuse / 4096
                    + channel(self.material[1]) * specular / 4096;
                *out += channel(self.lights[i][1]) * material / 31;
            }
        }
        self.color = color.iter().enumerate().fold(0u16, |v, (c, x)| {
            v | (((*x).clamp(0, 31) as u16) << (c * 5))
        });
    }

    fn texture_from_vector(&mut self, vector: [i32; 3]) {
        for c in 0..2 {
            let value = (0..3)
                .map(|j| vector[j] as i64 * self.tex[j][c] as i64)
                .sum::<i64>();
            self.st[c] = ((value >> 20) + self.raw_st[c] as i64) as i16;
        }
    }

    fn vertex(&mut self) {
        let Some(kind) = self.prim else { return };
        if self.tex_param >> 30 == 3 {
            self.texture_from_vector(self.vtx);
        }
        let v = [self.vtx[0], self.vtx[1], self.vtx[2], 4096];
        let clip = xform(xform(v, &self.pos), &self.proj);
        self.verts.push(Vertex {
            clip,
            color: self.color,
            st: self.st,
        });
        let n = self.verts.len();
        let emit = |g: &mut Gx, idx: &[usize]| {
            let verts = idx.iter().map(|&i| g.verts[i]).collect();
            g.polygons.push(Polygon {
                verts,
                attr: g.attr,
                tex_param: g.tex_param,
                pltt_base: g.pltt_base,
            });
        };
        match kind {
            0 if n == 3 => {
                emit(self, &[0, 1, 2]);
                self.verts.clear();
            }
            1 if n == 4 => {
                emit(self, &[0, 1, 2, 3]);
                self.verts.clear();
            }
            2 if n >= 3 => {
                // triangle strip: every other triangle is flipped to keep the winding
                if self.strip_odd {
                    emit(self, &[n - 2, n - 3, n - 1]);
                } else {
                    emit(self, &[n - 3, n - 2, n - 1]);
                }
                self.strip_odd = !self.strip_odd;
            }
            3 if n >= 4 && n % 2 == 0 => emit(self, &[n - 4, n - 3, n - 1, n - 2]),
            _ => {}
        }
    }
}

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

fn mat4(p: &[u32]) -> Mat {
    let mut m = [[0; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            m[i][j] = p[i * 4 + j] as i32;
        }
    }
    m
}

fn mat43(p: &[u32]) -> Mat {
    let mut m = IDENTITY;
    for i in 0..4 {
        for j in 0..3 {
            m[i][j] = p[i * 3 + j] as i32;
        }
        m[i][3] = 0;
    }
    m[3][3] = 4096;
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_transforms_latch_on_their_source_command() {
        let mut g = Gx::new();
        g.tex[0][0] = 8192;
        g.tex[3][0] = 16 * 4096; // one texel in mode 1's 1/16 translation convention
        g.exec(0x2a, &[1 << 30]);
        g.exec(0x22, &[32 | (48 << 16)]);
        assert_eq!(g.st, [80, 48]);
        g.tex[0][0] = 4096;
        g.exec(0x2a, &[2 << 30]);
        g.exec(0x22, &[32 | (48 << 16)]);
        g.exec(0x21, &[256]); // +0.5 X -> +8 in fx4
        assert_eq!(g.st, [40, 48]);
        g.exec(0x2a, &[3 << 30]);
        g.exec(0x40, &[0]);
        g.exec(0x23, &[4096, 0]);
        assert_eq!(g.verts[0].st, [48, 48]);
    }

    #[test]
    fn lighting_uses_material_enabled_lights_and_normal_matrix() {
        let mut g = Gx::new();
        g.exec(0x30, &[0x8000 | 0x001f]); // diffuse -> current vertex colour
        assert_eq!(g.color, 0x001f);
        g.exec(0x31, &[0x03e0 << 16]); // emission remains when all lights disabled
        g.exec(0x21, &[511 << 20]);
        assert_eq!(g.color, 0x03e0);
        g.exec(0x31, &[0]);
        g.exec(0x29, &[1]);
        g.exec(0x32, &[512 << 20]); // light points down -Z
        g.exec(0x33, &[0x7fff]);
        g.exec(0x21, &[511 << 20]); // normal faces +Z
        assert!(g.color & 31 >= 30);
        assert_eq!(g.color & !31, 0);
        g.exec(0x21, &[512 << 20]); // back-facing normal receives no diffuse
        assert_eq!(g.color, 0);
        g.exec(0x10, &[2]);
        let directions = g.vecm;
        g.exec(0x1b, &[8192, 8192, 8192]);
        assert_eq!(
            g.vecm, directions,
            "position scaling must not scale lighting normals"
        );
        assert_eq!(g.pos[0][0], 8192);
    }

    #[test]
    fn relative_vertices_use_twelve_fraction_bits_not_normal_vector_units() {
        let mut g = Gx::new();
        g.write(0x500, 0); // three vertices
        g.write(0x48c, 0x1000_1000); // full coordinate: 1,1,1
        g.write(0x48c, 0x1000);
        // Signed 10-bit deltas in 1/4096 units: +1,-1,+511.
        g.write(0x4a0, 1 | (0x3ff << 10) | (0x1ff << 20));
        // -512,+2,-2, accumulated from the immediately preceding vertex.
        g.write(0x4a0, 0x200 | (2 << 10) | (0x3fe << 20));
        assert_eq!(g.polygons.len(), 1);
        assert_eq!(g.polygons[0].verts[1].clip, [4097, 4095, 4607, 4096]);
        assert_eq!(g.polygons[0].verts[2].clip, [3585, 4097, 4605, 4096]);
    }

    fn packed(g: &mut Gx, cmds: &[u8], params: &[u32]) {
        let mut w = 0u32;
        for (i, c) in cmds.iter().enumerate() {
            w |= (*c as u32) << (8 * i);
        }
        g.write(0x400, w);
        for p in params {
            g.write(0x400, *p);
        }
    }

    #[test]
    fn packed_fifo_triangle_with_identity() {
        let mut g = Gx::new();
        // BEGIN_VTXS(0), VTX_16 x3 then END_VTXS ; params: 1 + 3*2
        packed(
            &mut g,
            &[0x40, 0x23, 0x23, 0x23],
            &[0, 0, 0, 0x0000_1000, 0, 0x2000_1000, 0],
        );
        packed(&mut g, &[0x41], &[]);
        assert_eq!(g.polygons.len(), 1);
        let p = &g.polygons[0];
        assert_eq!(p.verts.len(), 3);
        assert_eq!(p.verts[1].clip, [0x1000, 0, 0, 4096]);
        assert_eq!(p.verts[2].clip[0..2], [0x1000, 0x2000]);
    }

    #[test]
    fn translate_scale_and_stack() {
        let mut g = Gx::new();
        g.write(0x440 - 0x30 + 0x0, 0); // harmless: below the command ports, ignored
        g.write(0x400 + 0x10 * 4, 1); // MTX_MODE position (direct port 0x10)
        g.write(0x400 + 0x1C * 4, 0x2000); // MTX_TRANS x
        g.write(0x400 + 0x1C * 4, 0);
        g.write(0x400 + 0x1C * 4, 0);
        g.write(0x400 + 0x11 * 4, 0); // push
        g.write(0x400 + 0x1B * 4, 0x2000); // scale x2
        g.write(0x400 + 0x1B * 4, 0x2000);
        g.write(0x400 + 0x1B * 4, 0x2000);
        g.write(0x400 + 0x40 * 4, 0);
        g.write(0x400 + 0x23 * 4, 0x0000_1000); // x=0x1000 y=0
        g.write(0x400 + 0x23 * 4, 0);
        let v = g.polygons.len();
        assert_eq!(v, 0); // one vertex only so far
        // (x*2) + translation: scale was applied to the left of the translation, so the vertex scales first
        g.write(0x400 + 0x12 * 4, 1); // pop restores the pre-scale matrix
        assert_eq!(g.pos[3][0], 0x2000);
        assert_eq!(g.pos[0][0], 4096);
    }

    #[test]
    fn viewport_maps_ndc_to_pixels() {
        let mut g = Gx::new();
        g.write(0x400 + 0x60 * 4, 0xBF_FF_00_00); // full screen 0,0 - 255,191
        let v = Vertex {
            clip: [0, 0, 0, 4096],
            ..Default::default()
        };
        assert_eq!(g.screen(&v), Some((128.0, 95.0, 0.5)));
        // top-left corner of NDC lands on pixel column 0 and one line above the top row (y is row 191 - 192)
        let left_top = Vertex {
            clip: [-4096, 4096, -4096, 4096],
            ..Default::default()
        };
        assert_eq!(g.screen(&left_top), Some((0.0, -1.0, 0.0)));
        assert_eq!(
            g.screen(&Vertex {
                clip: [0, 0, 0, 0],
                ..Default::default()
            }),
            None
        );
    }

    #[test]
    fn strips_keep_winding_and_quads() {
        let mut g = Gx::new();
        g.write(0x400 + 0x40 * 4, 2); // triangle strip
        for x in 0..4 {
            g.write(0x400 + 0x25 * 4, x << 12);
        }
        assert_eq!(g.polygons.len(), 2);
        // second triangle is (v2, v1, v3)
        assert_eq!(g.polygons[1].verts[0].clip[0], 2 << 12);
        assert_eq!(g.polygons[1].verts[1].clip[0], 1 << 12);
        g.write(0x400 + 0x40 * 4, 3); // quad strip: 4 vertices -> one quad (0,1,3,2)
        g.polygons.clear();
        for x in 0..4 {
            g.write(0x400 + 0x25 * 4, x << 12);
        }
        assert_eq!(g.polygons.len(), 1);
        assert_eq!(g.polygons[0].verts[2].clip[0], 3 << 12);
        assert_eq!(g.polygons[0].verts[3].clip[0], 2 << 12);
    }

    #[test]
    fn projection_and_unknown_commands() {
        let mut g = Gx::new();
        g.write(0x400 + 0x10 * 4, 0); // projection mode
        g.write(0x400 + 0x1B * 4, 0x800); // scale 0.5
        g.write(0x400 + 0x1B * 4, 0x800);
        g.write(0x400 + 0x1B * 4, 0x800);
        g.write(0x400 + 0x40 * 4, 0);
        for _ in 0..3 {
            g.write(0x400 + 0x24 * 4, 0x40 | 0x40 << 10); // VTX_10 x=1.0 y=1.0 (0x40<<6 = 4096)
        }
        assert_eq!(
            g.polygons[0].verts[0].clip,
            [2048, 2048, 0, 2048 * 0 + 4096]
        );
        packed(&mut g, &[0xFE], &[]);
        assert_eq!(g.unknown, vec![0xFE]);
    }
}

#[cfg(test)]
mod toon_tests {
    use super::*;

    #[test]
    fn toon_polygons_take_their_colour_from_the_table_once() {
        let mut io = vec![0u8; 0x400];
        io[0x380 + 2 * 5..0x380 + 2 * 5 + 2].copy_from_slice(&0x7c1fu16.to_le_bytes());
        let v = |color| Vertex { clip: [0; 4], color, st: [0; 2] };
        let mut polys = vec![
            Polygon { verts: vec![v(5), v(0x03e5)], attr: 2 << 4 | 0x1f << 16, tex_param: 0, pltt_base: 0 },
            Polygon { verts: vec![v(5)], attr: 0, tex_param: 0, pltt_base: 0 },
        ];
        resolve_toon(&mut polys, &io);
        assert_eq!((polys[0].verts[0].color, polys[0].verts[1].color), (0x7c1f, 0x7c1f), "red 5 -> entry 5");
        assert_eq!(polys[0].attr, 0x1f << 16, "now modulation");
        assert_eq!(polys[1].verts[0].color, 5, "other modes unchanged");
        let again = polys.clone();
        resolve_toon(&mut polys, &io);
        assert_eq!(polys, again, "applying twice changes nothing");
        io[0x60] = 2; // highlight shading: left alone
        let mut hl = vec![Polygon { verts: vec![v(5)], attr: 2 << 4, tex_param: 0, pltt_base: 0 }];
        resolve_toon(&mut hl, &io);
        assert_eq!(hl[0].verts[0].color, 5);
    }
}
