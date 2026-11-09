use recomp::{
    native,
    native::math::*,
    rt::{self, Cpu, Mem},
};
use std::{hint::black_box, time::Instant};

// Dense RAM avoids benchmarking the differential oracle's BTreeMap journal.
struct BenchBus {
    words: [u32; 256],
    hw: recomp::ds_mem::Bus,
}
impl Mem for BenchBus {
    fn r8(&mut self, a: u32) -> u8 {
        (self.r32(a & !3) >> ((a & 3) * 8)) as u8
    }
    fn r16(&mut self, a: u32) -> u16 {
        if (0x0400_0280..0x0400_02c0).contains(&a) {
            self.hw.r16(a)
        } else {
            (self.r32(a & !3) >> ((a & 2) * 8)) as u16
        }
    }
    fn r32(&mut self, a: u32) -> u32 {
        if (0x0400_0280..0x0400_02c0).contains(&a) {
            self.hw.r32(a)
        } else {
            self.words[(a as usize / 4) & 255]
        }
    }
    fn w8(&mut self, a: u32, v: u8) {
        let shift = (a & 3) * 8;
        let old = self.r32(a & !3);
        self.w32(a & !3, (old & !(255 << shift)) | ((v as u32) << shift));
    }
    fn w16(&mut self, a: u32, v: u16) {
        if (0x0400_0280..0x0400_02c0).contains(&a) {
            self.hw.w16(a, v);
        } else {
            self.w8(a, v as u8);
            self.w8(a + 1, (v >> 8) as u8);
        }
    }
    fn w32(&mut self, a: u32, v: u32) {
        if (0x0400_0280..0x0400_02c0).contains(&a) {
            self.hw.w32(a, v);
        } else {
            self.words[(a as usize / 4) & 255] = v;
        }
    }
}
impl rt::Bus for BenchBus {
    fn call(&mut self, _: &mut Cpu, _: u32, _: u32) -> u32 {
        panic!("not a leaf")
    }
    fn swi(&mut self, _: &mut Cpu, _: u32) {
        panic!("unexpected SWI")
    }
    fn mrc(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8) -> u32 {
        panic!("unexpected MRC")
    }
    fn mcr(&mut self, _: &mut Cpu, _: u8, _: u8, _: u8, _: u8, _: u8, _: u32) {
        panic!("unexpected MCR")
    }
}

fn median(mut run: impl FnMut()) -> f64 {
    let mut times = [0.0; 5];
    for sample in &mut times {
        let now = Instant::now();
        for _ in 0..100_000 {
            run();
        }
        *sample = now.elapsed().as_secs_f64() * 1e9 / 100_000.0;
    }
    times.sort_by(f64::total_cmp);
    times[2]
}

fn main() {
    native::BUDGET_FREE.store(true, std::sync::atomic::Ordering::Relaxed);
    let a = VecFx32 {
        x: 123456,
        y: -67890,
        z: 45678,
    };
    let b = VecFx32 {
        x: -34567,
        y: 89012,
        z: 23456,
    };
    println!("entry,name,core_ns,adapter_ns,lifted_ns");
    for &(entry, lifted) in LIFTED {
        let n = native::NATIVE.iter().find(|n| n.entry == entry).unwrap();
        let core = match entry {
            0x020738f8 => median(|| {
                black_box(fx_abs(black_box(-123456)));
            }),
            0x02091f44 => median(|| {
                black_box(fx_mul_trunc(black_box(a.x), black_box(b.x)));
            }),
            0x021488e8 => median(|| {
                black_box(fx_mul(black_box(a.x), black_box(b.x)));
            }),
            0x02039828 => median(|| {
                black_box(vec_dot_trunc(black_box(a), black_box(b)));
            }),
            0x0214848c => median(|| {
                black_box(vec_dot(black_box(a), black_box(b)));
            }),
            0x020397a8 => median(|| {
                black_box(vec_cross(black_box(a), black_box(b)));
            }),
            0x02147f50 => median(|| {
                black_box(vec_distance(black_box(a), black_box(b)));
            }),
            0x02146c98 => median(|| {
                black_box(mtx_identity22());
            }),
            0x02146cbc => median(|| {
                black_box(mtx_identity33());
            }),
            0x021471bc => median(|| {
                black_box(mtx_identity43());
            }),
            0x02147954 => median(|| {
                black_box(mtx_identity44());
            }),
            0x02147cd8 => median(|| {
                black_box(div_s32(black_box(a.x), black_box(b.x)).remainder);
            }),
            0x02147d30 => median(|| {
                black_box(div_s32(black_box(a.x), black_box(b.x)).quotient);
            }),
            _ => unreachable!(),
        };
        let mut bus = BenchBus {
            words: [0; 256],
            hw: recomp::ds_mem::Bus::new(),
        };
        for (i, value) in [a.x, a.y, a.z].into_iter().enumerate() {
            bus.words[64 + i] = value as u32;
        }
        for (i, value) in [b.x, b.y, b.z].into_iter().enumerate() {
            bus.words[80 + i] = value as u32;
        }
        let mut start = Cpu::default();
        start.budget = u32::MAX / 2;
        start.r[13] = 0x300;
        start.r[14] = 0x0100_0000;
        start.r[0] = 0x100;
        start.r[1] = 0x140;
        start.r[2] = 0x180;
        if matches!(
            entry,
            0x020738f8 | 0x02091f44 | 0x021488e8 | 0x02147cd8 | 0x02147d30
        ) {
            start.r[0] = a.x as u32;
            start.r[1] = b.x as u32;
        }
        let mut cpu = start.clone();
        let adapter = black_box(n.f);
        let adapter_ns = median(|| {
            cpu = black_box(start.clone());
            black_box(adapter(&mut cpu, &mut bus, entry));
            black_box(&cpu);
        });
        let lifted = black_box(lifted);
        let lifted_ns = median(|| {
            cpu = black_box(start.clone());
            black_box(lifted(&mut cpu, &mut bus, entry));
            black_box(&cpu);
        });
        println!(
            "{entry:#010x},{},{core:.2},{adapter_ns:.2},{lifted_ns:.2}",
            n.name
        );
    }
}
