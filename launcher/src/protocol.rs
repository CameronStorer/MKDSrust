//! `mkdsrust://install?url=...` links from mkdsrust.com: registered per user (HKCU, no admin rights), and only
//! https downloads from mkdsrust.com are accepted. The downloaded zip goes through the normal screening.
use std::{path::Path, process::Command};

const KEY: &str = r"HKCU\Software\Classes\mkdsrust";

fn hidden(c: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c
}

/// The command Windows runs for `mkdsrust://` links.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Register (or repair) the handler for this launcher. Cheap when it is already correct.
pub fn register(exe: &Path) -> Result<bool, String> {
    let wanted = command_for(exe);
    let current = hidden(Command::new("reg.exe").args(["query", &format!(r"{KEY}\shell\open\command"), "/ve"]))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if current.contains(&wanted) {
        return Ok(false);
    }
    let add = |key: String, name: Option<&str>, value: &str| -> Result<(), String> {
        let mut c = Command::new("reg.exe");
        c.args(["add", &key]);
        match name {
            Some(n) => c.args(["/v", n]),
            None => c.arg("/ve"),
        };
        let ok = hidden(c.args(["/t", "REG_SZ", "/d", value, "/f"])).status().map_err(|e| e.to_string())?.success();
        if ok { Ok(()) } else { Err(format!("reg add {key} failed")) }
    };
    add(KEY.into(), None, "URL:MKDSrust mod install")?;
    add(KEY.into(), Some("URL Protocol"), "")?;
    add(format!(r"{KEY}\DefaultIcon"), None, &format!("\"{}\",0", exe.display()))?;
    add(format!(r"{KEY}\shell\open\command"), None, &wanted)?;
    Ok(true)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Accept only `https://mkdsrust.com/...` (or a subdomain), without credentials, odd ports or whitespace.
pub fn trusted_download(url: &str) -> Result<&str, String> {
    let rest = url.strip_prefix("https://").ok_or("Only https links from mkdsrust.com are accepted")?;
    if url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\') {
        return Err("The link contains characters that are not allowed".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err("Links with user names are not accepted".into());
    }
    let host = match authority.rsplit_once(':') {
        Some((h, "443")) => h,
        Some(_) => return Err("Links with a port are not accepted".into()),
        None => authority,
    }
    .to_ascii_lowercase();
    if host == "mkdsrust.com" || host.ends_with(".mkdsrust.com") {
        Ok(url)
    } else {
        Err(format!("Mods can only be installed from mkdsrust.com (this link points to {host})"))
    }
}

/// The download URL from a `mkdsrust://install?url=<encoded https URL>` link.
pub fn install_url(link: &str) -> Result<String, String> {
    let lower = link.to_ascii_lowercase();
    let rest = ["mkdsrust://install/?", "mkdsrust://install?", "mkdsrust:install?"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .map(|p| &link[p.len()..])
        .ok_or("Not an MKDSrust install link")?;
    let raw = rest
        .split('&')
        .find_map(|kv| kv.strip_prefix("url="))
        .ok_or("The install link has no url")?;
    let url = percent_decode(raw).ok_or("The install link is damaged")?;
    trusted_download(&url)?;
    Ok(url)
}

/// Download a mod zip (≤100 MB) with Windows' own curl, https only, also for redirects.
pub fn download(url: &str, dest: &Path) -> Result<(), String> {
    trusted_download(url)?;
    let out = hidden(Command::new("curl.exe").args([
        "-fsSL",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--max-filesize",
        "104857600",
        "--max-time",
        "600",
        "-A",
        concat!("MKDSrust-launcher/", env!("CARGO_PKG_VERSION")),
        "-o",
    ]))
    .arg(dest)
    .arg(url)
    .output()
    .map_err(|e| format!("Could not start the download: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Download failed: {}. If the link is old, click \"Install with MKDSrust\" on the site again.",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let head = std::fs::read(dest).map(|b| b.get(..2) == Some(b"PK")).unwrap_or(false);
    if !head {
        return Err("The site did not send a mod package (sign in on mkdsrust.com, then click Install again).".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_links_accept_only_https_mkdsrust_downloads() {
        let ok = "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%2Fapi%2Fdownload%2F12%3Ft%3Dabc";
        assert_eq!(install_url(ok).unwrap(), "https://mkdsrust.com/api/download/12?t=abc");
        assert!(install_url("mkdsrust://install/?url=https://www.mkdsrust.com/x.zip").is_ok());
        assert!(install_url("MKDSRUST://install?url=https://mkdsrust.com:443/x").is_ok());
        for bad in [
            "mkdsrust://install?url=http%3A%2F%2Fmkdsrust.com%2Fx",
            "mkdsrust://install?url=https%3A%2F%2Fevil.com%2Fx",
            "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com.evil.com%2Fx",
            "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%40evil.com%2Fx",
            "mkdsrust://install?url=https%3A%2F%2Fevil.com%2F%40mkdsrust.com",
            "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%3A8080%2Fx",
            "mkdsrust://install?url=https%3A%2F%2Fmkdsrust.com%2Fa%20b",
            "mkdsrust://install?url=file%3A%2F%2FC%3A%2Fx",
            "mkdsrust://uninstall?url=https%3A%2F%2Fmkdsrust.com%2Fx",
            "mkdsrust://install?url=%ZZ",
            "https://mkdsrust.com/x",
        ] {
            assert!(install_url(bad).is_err(), "{bad}");
        }
        assert_eq!(command_for(Path::new(r"C:\a b\MKDSrust.exe")), r#""C:\a b\MKDSrust.exe" "%1""#);
    }
}
