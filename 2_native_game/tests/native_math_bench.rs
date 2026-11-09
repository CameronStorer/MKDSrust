//! Opt-in benchmark: generates the user's lifted leaves in private scratch, never in the repo.
#![cfg(feature = "gen")]
use std::{path::PathBuf, process::Command};

#[test]
#[ignore = "set MKDS_ROM and MKDS_MATH_SCRATCH; reports core/adapter/lifted ns per call"]
fn math_pilot_benchmark() {
    let rom = std::fs::read(std::env::var_os("MKDS_ROM").expect("MKDS_ROM")).unwrap();
    let sections = mkds_builder::extract::arm9_sections(&rom).unwrap();
    let scratch =
        PathBuf::from(std::env::var_os("MKDS_MATH_SCRATCH").expect("private MKDS_MATH_SCRATCH"));
    let dir = scratch.join("math_bench");
    std::fs::create_dir_all(&dir).unwrap();
    let mut source = String::from("#![allow(unused, non_snake_case)]\n");
    let mut table = String::new();
    for n in recomp::native::NATIVE
        .iter()
        .filter(|n| recomp::native::math::ENTRIES.contains(&n.entry))
    {
        let section = sections.iter().find(|s| s.name == n.segment).unwrap();
        let image = recomp::lift::Image::new(section.ram_address, &section.data);
        let function = recomp::lift::discover(&image, n.entry, None, 4096).unwrap();
        assert!(function.undecoded.is_empty());
        assert!(function.insts.values().all(|d| !d.inst.is_call()));
        let name = format!("math_{:08x}", n.entry);
        // No instruction budget, so this measures the lifted arithmetic and ABI itself.
        source += &recomp::lift::emit_full(&function, &name, Some(&image), Some(&|_, _| 0), true);
        table += &format!("({}, f_{} as recomp::mem::LiftedFn),\n", n.entry, name);
    }
    source += &format!("static LIFTED: &[(u32, recomp::mem::LiftedFn)] = &[{table}];\n");
    source += include_str!("support/math_bench.rs");
    let src = dir.join("bench.rs");
    let exe = dir.join(format!("bench{}", std::env::consts::EXE_SUFFIX));
    std::fs::write(&src, source).unwrap();
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
        .unwrap()
        .path();
    let mut cmd = Command::new("rustc");
    cmd.args(["--edition=2024", "-O"])
        .arg(&src)
        .arg("--extern")
        .arg(format!("recomp={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", deps.display()))
        .arg("-o")
        .arg(&exe);
    for b in std::fs::read_dir(deps.parent().unwrap().join("build"))
        .unwrap()
        .filter_map(Result::ok)
    {
        if let Ok(output) = std::fs::read_to_string(b.path().join("output")) {
            for line in output.lines() {
                if let Some(p) = line
                    .strip_prefix("cargo:rustc-link-search=native=")
                    .or_else(|| line.strip_prefix("cargo::rustc-link-search=native="))
                {
                    cmd.arg("-L").arg(format!("native={p}"));
                }
            }
        }
    }
    let compile = cmd.output().unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&exe).output().unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let output = String::from_utf8(run.stdout).unwrap();
    std::fs::write(dir.join("results.csv"), &output).unwrap();
    eprintln!("{output}");
}
