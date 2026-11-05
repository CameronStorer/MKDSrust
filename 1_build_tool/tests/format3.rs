#![cfg(feature = "diff")]
mod common;
use common::*;
use mkds_builder::{
    dsfiles, modding,
    modpkg::{self, Entry, archive_delta, overlay},
};
use std::collections::BTreeMap;

fn apply(bytes: &[u8], contents: &overlay::Files) -> modpkg::Result<Vec<u8>> {
    let m = manifest(contents);
    let Entry::Archive(a) = m.files.values().next().unwrap() else {
        panic!()
    };
    archive_delta::apply(bytes, &a.archive, contents)
}

#[test]
fn nested_carc_named_folders_compressed_leaves_and_untouched_archives() {
    let leaf = dsfiles::lz77_compress(b"RGCN synthetic pixels");
    let inner = dsfiles::lz77_compress(&narc(&[
        ("art/image.ncgr", leaf),
        ("keep.bmg", b"MESGbmg1 untouched".to_vec()),
    ]));
    let untouched =
        dsfiles::lz77_compress(&narc(&[("keep.bmg", b"MESGbmg1 exact bytes".to_vec())]));
    let root = dsfiles::lz77_compress(&narc(&[
        ("sub/inner.carc", inner),
        ("unchanged.carc", untouched.clone()),
    ]));
    let p = package(
        "test.nested",
        "a.carc",
        &[(
            "sub/inner.carc/art/image.ncgr",
            b"RGCN synthetic pixels",
            b"RGCN new pixels with a different size",
        )],
    );
    let out = apply(&root, &p).unwrap();
    assert_eq!(out[0], 0x10);
    let inner = member(&out, "sub/inner.carc");
    assert_eq!(inner[0], 0x10);
    let leaf = member(&inner, "art/image.ncgr");
    assert_eq!(leaf[0], 0x10);
    assert_eq!(plain(&leaf), b"RGCN new pixels with a different size");
    assert_eq!(member(&inner, "keep.bmg"), b"MESGbmg1 untouched");
    assert_eq!(member(&out, "unchanged.carc"), untouched);
}

#[test]
fn disjoint_mods_combine_conflicts_are_atomic_and_cache_tracks_inputs_and_payloads() {
    let root = narc(&[("a.bmg", b"old a".to_vec()), ("b.bmg", b"old b".to_vec())]);
    let originals = BTreeMap::from([("a.narc".into(), root)]);
    let first = package("test.first", "a.narc", &[("a.bmg", b"old a", b"first a")]);
    let second = package("test.second", "a.narc", &[("b.bmg", b"old b", b"second b")]);
    let cache = tempfile::tempdir().unwrap();
    let stage = |p: &overlay::Files, current: &overlay::Files| {
        overlay::stage(&manifest(p), p, &originals, current, Some(cache.path()))
    };
    let mut current = originals.clone();
    current.extend(stage(&first, &current).unwrap());
    current.extend(stage(&second, &current).unwrap());
    assert_eq!(member(&current["a.narc"], "a.bmg"), b"first a");
    assert_eq!(member(&current["a.narc"], "b.bmg"), b"second b");
    // Cached second mod against the original must preserve the original first member.
    let alone = stage(&second, &originals).unwrap();
    assert_eq!(member(&alone["a.narc"], "a.bmg"), b"old a");
    let conflict = package(
        "test.conflict",
        "a.narc",
        &[
            ("b.bmg", b"old b", b"bad b"),
            ("a.bmg", b"first a", b"bad a"),
        ],
    );
    let snapshot = current.clone();
    let error = stage(&conflict, &current).unwrap_err().to_string();
    assert!(
        error.contains("b.bmg") && error.contains("source member hash"),
        "{error}"
    );
    assert_eq!(current, snapshot);
    let chained = package(
        "test.chain",
        "a.narc",
        &[("a.bmg", b"first a", b"chained a")],
    );
    assert_eq!(
        member(&stage(&chained, &current).unwrap()["a.narc"], "a.bmg"),
        b"chained a"
    );
    // Same metadata and input, changed patch bytes: cached success must not hide failure.
    let mut corrupt = second.clone();
    corrupt.get_mut("patches/0.bsdiff").unwrap()[0] ^= 1;
    assert!(stage(&corrupt, &originals).is_err());
    // Hash checks are part of the key too.
    let mut corrupt = second.clone();
    edit_manifest(&mut corrupt, |m| {
        m["files"]["a.narc"]["archive"]["b.bmg"]["target_sha256"] = "0".repeat(64).into()
    });
    assert!(
        stage(&corrupt, &originals)
            .unwrap_err()
            .to_string()
            .contains("b.bmg")
    );
}

#[test]
fn errors_name_missing_members_wrong_sources_targets_and_payloads() {
    let root = narc(&[("a.bmg", b"old".to_vec())]);
    for (path, before, after, expected) in [
        (
            "missing.bmg",
            b"old".as_slice(),
            b"new".as_slice(),
            "missing member",
        ),
        (
            "a.bmg",
            b"wrong".as_slice(),
            b"new".as_slice(),
            "source member hash",
        ),
    ] {
        let error = apply(
            &root,
            &package("test.bad", "a.narc", &[(path, before, after)]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(path) && error.contains(expected), "{error}");
    }
    let mut p = package("test.bad", "a.narc", &[("a.bmg", b"old", b"new")]);
    p.remove("patches/0.bsdiff");
    assert!(apply(&root, &p).unwrap_err().to_string().contains("a.bmg"));
    p.insert("patches/0.bsdiff".into(), patch(b"old", b"wrong"));
    assert!(
        apply(&root, &p)
            .unwrap_err()
            .to_string()
            .contains("a.bmg: patched member hash")
    );
}

#[test]
fn strict_schema_and_all_member_payload_paths() {
    let original = package(
        "test.schema",
        "a.narc",
        &[("a.bmg", b"old", b"new"), ("b.bmg", b"old", b"new")],
    );
    for format in [0, 1, 2, 4] {
        let mut p = original.clone();
        edit_manifest(&mut p, |m| m["format"] = format.into());
        assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
    }
    let bad_members = [
        serde_json::json!({}),
        serde_json::json!({"a.bmg":{"patch":"p.bsdiff","source_sha256":"0".repeat(64),"target_sha256":"F".repeat(64)}}),
        serde_json::json!({"../a.bmg":{"patch":"p.bsdiff","source_sha256":"0".repeat(64),"target_sha256":"0".repeat(64)}}),
    ];
    for members in bad_members {
        let mut p = original.clone();
        edit_manifest(&mut p, |m| m["files"]["a.narc"]["archive"] = members);
        assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
    }
    for bad in [
        "../escape.bsdiff",
        "/abs.bsdiff",
        "C:/abs.bsdiff",
        "bad\\path.bsdiff",
        "p.exe",
        "manifest.json",
    ] {
        let mut p = original.clone();
        edit_manifest(&mut p, |m| {
            m["files"]["a.narc"]["archive"]["b.bmg"]["patch"] = bad.into()
        });
        assert!(
            modpkg::Manifest::parse(&p["manifest.json"]).is_err(),
            "{bad}"
        );
    }
    for other in ["A.bmg", "a.bmg/sub.bmg"] {
        let mut p = original.clone();
        edit_manifest(&mut p, |m| {
            let record = m["files"]["a.narc"]["archive"]["a.bmg"].clone();
            m["files"]["a.narc"]["archive"][other] = record;
        });
        assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
    }
    for field in ["extra", "add", "patch"] {
        let mut p = original.clone();
        edit_manifest(&mut p, |m| m["files"]["a.narc"][field] = "p.bsdiff".into());
        assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
    }
    let mut p = original.clone();
    edit_manifest(&mut p, |m| {
        m["files"]["a.narc"]["archive"]["a.bmg"]["unknown"] = true.into()
    });
    assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
    let text = String::from_utf8(original["manifest.json"].clone()).unwrap();
    let duplicate = text.replace("\"archive\":{", "\"archive\":{\"a.bmg\":{\"patch\":\"p.bsdiff\",\"source_sha256\":\"0\",\"target_sha256\":\"0\"},");
    assert!(modpkg::Manifest::parse(duplicate.as_bytes()).is_err());
}

#[test]
fn archive_depth_and_allocation_hints_are_bounded() {
    for depth in [8, 9] {
        let mut root = narc(&[("a.bmg", b"old".to_vec())]);
        let mut path = "a.bmg".to_string();
        for _ in 0..depth {
            root = narc(&[("inner.narc", root)]);
            path = format!("inner.narc/{path}");
        }
        let result = apply(
            &root,
            &package("test.depth", "a.narc", &[(&path, b"old", b"new")]),
        );
        if depth == 8 {
            assert!(result.is_ok());
        } else {
            assert!(result.unwrap_err().to_string().contains(&path));
        }
    }
    let mut p = package("test.limit", "a.narc", &[("a.bmg", b"old", b"new")]);
    let root = narc(&[("a.bmg", b"old".to_vec())]);
    p.get_mut("patches/0.bsdiff").unwrap()[24..32]
        .copy_from_slice(&((archive_delta::MAX_MEMBER + 1) as u64).to_le_bytes());
    assert!(apply(&root, &p).unwrap_err().to_string().contains("a.bmg"));
    // A forged small hint is also bounded during actual writes.
    p.get_mut("patches/0.bsdiff").unwrap()[24..32].copy_from_slice(&1u64.to_le_bytes());
    assert!(
        apply(&root, &p)
            .unwrap_err()
            .to_string()
            .contains("declared output size")
    );
    let mut compressed = vec![0x10, 0, 0, 0];
    compressed.extend_from_slice(&((archive_delta::MAX_MEMBER + 1) as u32).to_le_bytes());
    let root = narc(&[("a.bmg", compressed)]);
    assert!(apply(&root, &p).unwrap_err().to_string().contains("a.bmg"));
}

#[test]
fn ambiguous_archive_names_and_oversized_repacking_fail_before_commit() {
    let p = package("test.names", "a.narc", &[("a.bmg", b"old", b"new")]);
    let duplicate = narc(&[("a.bmg", b"old".to_vec()), ("A.bmg", b"other".to_vec())]);
    let error = apply(&duplicate, &p).unwrap_err().to_string();
    assert!(
        error.contains("a.bmg") && error.contains("ambiguous"),
        "{error}"
    );
    let original = narc(&[("a.bmg", b"old".to_vec())]);
    let mut damaged = original.clone();
    let at = damaged.windows(5).position(|b| b == b"a.bmg").unwrap();
    damaged[at] = 0xff;
    assert!(
        apply(&damaged, &p)
            .unwrap_err()
            .to_string()
            .contains("a.bmg")
    );
    assert!(
        dsfiles::update_narc_bounded(&original, &[b"new".to_vec()], original.len() - 1).is_err()
    );
    assert!(dsfiles::update_narc_bounded(&original, &[vec![0; 1024]], 512).is_err());
}

#[test]
fn zip_install_uses_enabled_predecessors_and_failed_updates_leave_install_intact() {
    let root = narc(&[("a.bmg", b"old a".to_vec()), ("b.bmg", b"old b".to_vec())]);
    let image = rom(&[("a.narc", root)]);
    let game = tempfile::tempdir().unwrap();
    std::fs::write(game.path().join("manifest.json"), b"{}").unwrap();
    for (id, path, before, after) in [
        (
            "test.one",
            "a.bmg",
            b"old a".as_slice(),
            b"first a".as_slice(),
        ),
        ("test.two", "b.bmg", b"old b", b"second b"),
        ("test.chain", "a.bmg", b"first a", b"chained a"),
    ] {
        let p = package(id, "a.narc", &[(path, before, after)]);
        let zip_path = game.path().join(format!("{id}.zip"));
        zip(&p, &zip_path);
        modding::install_zip(game.path(), &image, &zip_path).unwrap();
        assert_eq!(
            overlay::read_payloads(&game.path().join("mods").join(id), &manifest(&p))
                .unwrap()
                .len(),
            1
        );
    }
    let order = modpkg::load_order(game.path());
    let enabled = modpkg::enabled(game.path());
    let old = std::fs::read(game.path().join("mods/test.two/manifest.json")).unwrap();
    let bad = package("test.two", "a.narc", &[("a.bmg", b"old a", b"conflict")]);
    let path = game.path().join("bad.zip");
    zip(&bad, &path);
    let error = modding::install_zip(game.path(), &image, &path)
        .unwrap_err()
        .to_string();
    assert!(error.contains("a.bmg"), "{error}");
    assert_eq!(
        std::fs::read(game.path().join("mods/test.two/manifest.json")).unwrap(),
        old
    );
    assert_eq!(modpkg::load_order(game.path()), order);
    assert_eq!(modpkg::enabled(game.path()), enabled);
    // Every payload is required, even when it is not the first member in an entry.
    let mut p = package(
        "test.missing",
        "a.narc",
        &[("a.bmg", b"old a", b"new a"), ("b.bmg", b"old b", b"new b")],
    );
    p.remove("patches/1.bsdiff");
    let retail = modpkg::RetailIndex::from_rom(&image).unwrap();
    assert!(modpkg::Package::validate(p, &retail).is_err());
}

#[test]
fn legacy_v1_v2_packages_and_v3_scripts_keep_existing_rules() {
    let originals = BTreeMap::from([("a.bmg".into(), b"old".to_vec())]);
    for format in [1, 2, 3] {
        let mut p = package("test.legacy", "a.narc", &[("a.bmg", b"old", b"new")]);
        edit_manifest(&mut p, |m| {
            m["format"] = format.into();
            m["files"] = serde_json::json!({"a.bmg":{"patch":"patches/0.bsdiff"}});
            if format >= 2 {
                m["scripts"] = serde_json::json!(["scripts/a.luau"]);
                m["api"] = 1.into();
            }
        });
        if format >= 2 {
            p.insert("scripts/a.luau".into(), b"print('synthetic')".to_vec());
        }
        let m = manifest(&p);
        let out = overlay::stage(&m, &p, &originals, &originals, None).unwrap();
        assert_eq!(out["a.bmg"], b"new");
        if format >= 2 {
            edit_manifest(&mut p, |m| m["api"] = 2.into());
            assert!(modpkg::Manifest::parse(&p["manifest.json"]).is_err());
        }
    }
}
