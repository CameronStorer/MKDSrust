//! Compile and execute small ORIGINAL test programs through the actual Rust
//! emitter. These cases do not depend on a ROM or the capped function inventory.
use recomp::lift::{Image, discover, emit};
use std::{path::PathBuf, process::Command};

#[test]
fn generated_edge_cases_match_independent_reference() {
    let cases: Vec<(&str, u32, Vec<u8>)> = vec![
        (
            "mrc_pc",
            0x100,
            [0xee11_ff10_u32, 0xe12f_ff1e]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect(),
        ),
        (
            "register_shift",
            0x100,
            [0xe1b0_0112_u32, 0xe12f_ff1e]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect(),
        ),
        (
            "signed_long",
            0x100,
            [0xe0d1_0392_u32, 0xe12f_ff1e]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect(),
        ),
        (
            "saturation",
            0x100,
            [0xe102_0051_u32, 0xe12f_ff1e]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect(),
        ),
        (
            "thumb_flags",
            0x101,
            vec![0x01, 0x20, 0x00, 0x38, 0x70, 0x47],
        ), // movs r0,#1; subs r0,#0; bx lr
    ];
    let mut generated = String::from("#![allow(non_upper_case_globals)]\n");
    let mut checks = String::new();
    for (name, entry, bytes) in cases {
        let image = Image::new(0x100, &bytes);
        let f = discover(&image, entry, Some((0x100, 0x100 + bytes.len() as u32)), 32).unwrap();
        assert!(
            f.undecoded.is_empty(),
            "test encoding was not decoded: {name}"
        );
        generated.push_str(&emit(&f, name));
        checks.push_str(&format!(r#"
        {{ let bytes: &[u8]=&{bytes:?};
        for seed in 0..64 {{
            let mut a=recomp::rt::Cpu::default();a.budget=100;a.r[14]=0x400;
            a.r[0]=0x7fff_ffff;a.r[1]=(seed*17) as u32;a.r[2]=0x8000_0001;a.r[3]=0xffff_fffe;
            a.c=seed&1!=0;a.v=seed&2!=0;a.q=seed&4!=0;
            let mut b=a.clone();let mut x=recomp::mem::Ram::new(bytes,0x100,seed);let mut y=recomp::mem::Ram::new(bytes,0x100,seed);
            let p=f_{name}(&mut a,&mut x,{entry});let q=recomp::interp::run(&mut b,&mut y,BLOCKS_{name},INSTS_{name},{entry});
            assert_eq!(p,q,"{name} exit seed{{seed}}");assert_eq!(a.state(),b.state(),"{name} state seed{{seed}}");
            assert_eq!(a.budget,b.budget,"{name} budget");assert_eq!(x.writes,y.writes,"{name} writes");assert_eq!(x.events,y.events,"{name} events");
        }} }}
        "#));
    }
    generated.push_str(&format!("fn main() {{ {checks} }}"));
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let library = std::fs::read_dir(&deps)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("librecomp-")
                && e.path().extension().is_some_and(|x| x == "rlib")
        })
        .max_by_key(|e| e.metadata().unwrap().modified().unwrap())
        .expect("compiled recomp library missing")
        .path();
    // Preserve generated test source for failure inspection; all files remain
    // under the ignored workspace scratchpad, with unique per-run names.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../8_scratch/recomp_synthetic_{}_{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("checks.rs");
    let exe = dir.join(format!("checks{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&source, generated).unwrap();
    // native library paths Cargo's build scripts added (e.g. the Windows import libraries of the
    // fiber crate's dependencies): plain rustc does not know them
    let mut native = Vec::new();
    if let Ok(builds) = std::fs::read_dir(deps.parent().unwrap().join("build")) {
        for b in builds.filter_map(Result::ok) {
            if let Ok(out) = std::fs::read_to_string(b.path().join("output")) {
                for l in out.lines() {
                    if let Some(p) = l.strip_prefix("cargo:rustc-link-search=native=").or_else(|| l.strip_prefix("cargo::rustc-link-search=native=")) {
                        native.push(format!("native={p}"));
                    }
                }
            }
        }
    }
    let compile = Command::new("rustc")
        .args(["--edition", "2024"])
        .arg(&source)
        .arg("--extern")
        .arg(format!("recomp={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .args(native.iter().flat_map(|n| ["-L", n.as_str()]))
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "generated source {} failed to compile:\n{}",
        source.display(),
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&exe).output().unwrap();
    assert!(
        run.status.success(),
        "synthetic generated-code mismatch; source {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&run.stderr)
    );
}
