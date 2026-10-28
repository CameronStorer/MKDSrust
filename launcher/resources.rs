//! Minimal Win32 .res writer. No downloaded resource crate or SDK resource compiler.
use std::{fs, path::Path};
fn word(b: &mut Vec<u8>, n: u16) {
    b.extend(n.to_le_bytes());
}
fn dword(b: &mut Vec<u8>, n: u32) {
    b.extend(n.to_le_bytes());
}
fn align(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}
fn text(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    dword(out, data.len() as u32);
    dword(out, 32);
    for n in [0xffff, kind, 0xffff, id] {
        word(out, n);
    }
    dword(out, 0);
    word(out, 0x1030);
    word(out, 0x409);
    dword(out, 0);
    dword(out, 0);
    out.extend(data);
    align(out);
}
fn block(key: &str, value: &[u8], value_len: u16, kind: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    word(&mut out, 0);
    word(&mut out, value_len);
    word(&mut out, kind);
    out.extend(text(key));
    align(&mut out);
    out.extend(value);
    for child in children {
        align(&mut out);
        out.extend(child);
    }
    let size = (out.len() as u16).to_le_bytes();
    out[..2].copy_from_slice(&size);
    out
}
pub fn write(icon: &Path, dest: &Path, version: &str) {
    let ico = fs::read(icon).expect("branding ICO");
    let count = u16::from_le_bytes(ico[4..6].try_into().unwrap());
    let mut out = Vec::new();
    resource(&mut out, 0, 0, &[]);
    let mut group = ico[..6].to_vec();
    for index in 0..count as usize {
        let entry = &ico[6 + index * 16..6 + (index + 1) * 16];
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        resource(&mut out, 3, index as u16 + 1, &ico[offset..offset + size]);
        group.extend(&entry[..12]);
        word(&mut group, index as u16 + 1);
    }
    resource(&mut out, 14, 1, &group);
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
    let high = (parts[0] << 16) | parts[1];
    let low = parts[2] << 16;
    let mut fixed = Vec::new();
    for n in [
        0xfeef04bd, 0x10000, high, low, high, low, 0x3f, 0, 0x40004, 1, 0, 0, 0,
    ] {
        dword(&mut fixed, n);
    }
    let mut strings = Vec::new();
    for (key, value) in [
        ("FileDescription", "MKDSrust desktop launcher"),
        ("ProductName", "MKDSrust"),
        ("FileVersion", version),
        ("ProductVersion", version),
        ("OriginalFilename", "MKDSrust.exe"),
        ("LegalCopyright", "GPL-3.0-or-later"),
    ] {
        let bytes = text(value);
        strings.push(block(key, &bytes, (bytes.len() / 2) as u16, 1, &[]));
    }
    let string_info = block(
        "StringFileInfo",
        &[],
        0,
        1,
        &[block("040904B0", &[], 0, 1, &strings)],
    );
    let translation = block("Translation", &[9, 4, 0xb0, 4], 4, 0, &[]);
    let var_info = block("VarFileInfo", &[], 0, 1, &[translation]);
    let info = block("VS_VERSION_INFO", &fixed, 52, 0, &[string_info, var_info]);
    resource(&mut out, 16, 1, &info);
    fs::write(dest, out).unwrap();
}
