use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
mod resources;

// Strict allowlist: never embed a ROM, generated workspace, target tree or game assets.
fn files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            if !matches!(
                e.file_name().to_str(),
                Some("target" | ".git" | "assets" | "source")
            ) {
                files(root, &p, out);
            }
        } else if matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("rs" | "toml" | "lock" | "csv" | "wgsl" | "md")
        ) {
            println!("cargo:rerun-if-changed={}", p.display());
            out.push(p.strip_prefix(root).unwrap().to_owned());
        }
    }
}
fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let res = out.join("branding.res");
        resources::write(
            &root.join("assets/branding/icon.ico"),
            &res,
            &env::var("CARGO_PKG_VERSION").unwrap(),
        );
        println!("cargo:rustc-link-arg={}", res.display());
    }
    let mut paths = Vec::new();
    for name in ["1_build_tool", "2_native_game", "dictionary"] {
        files(&root, &root.join(name), &mut paths);
    }
    paths.sort();
    fs::write(
        out.join("source-files.txt"),
        paths
            .iter()
            .map(|p| p.to_str().unwrap().replace('\\', "/"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let status = Command::new("tar")
        .current_dir(&root)
        .arg("-czf")
        .arg(out.join("source.tar.gz"))
        .arg("-T")
        .arg(out.join("source-files.txt"))
        .status()
        .expect("tar is required to package source");
    assert!(status.success(), "source packaging failed");
    println!("cargo:rerun-if-changed=../assets/branding");
}
