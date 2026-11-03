//! The mkdsrust.com launcher API (`docs/API.md` in the site repo): browsing, device-code sign-in, the member's
//! library and token downloads. HTTP goes through Windows' own `curl.exe` (like [`crate::protocol::download`]), so the
//! launcher needs no TLS stack of its own. The token is passed to curl on stdin, never on the command line, and is
//! never logged or shown.
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub const DEFAULT_SITE: &str = "https://mkdsrust.com";
/// Account file in the data folder: `{"token": "mkr_…", "username": "…"}`.
pub const ACCOUNT_FILE: &str = "account.json";
/// Written next to an installed mod's manifest when it came from the site (the manifest itself is strict).
pub const SOURCE_FILE: &str = ".mkdsrust-source.json";
const API_TIMEOUT: u64 = 30;
const DOWNLOAD_TIMEOUT: u64 = 600;

/// The site's base URL: `MKDSRUST_SITE` (for testing against a local site) or https://mkdsrust.com.
pub fn base() -> String {
    std::env::var("MKDSRUST_SITE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
        .unwrap_or_else(|| DEFAULT_SITE.into())
}

fn null_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A mod in a search result or the library.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModSummary {
    #[serde(deserialize_with = "null_default")]
    pub slug: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub category: String,
    #[serde(deserialize_with = "null_default")]
    pub author: String,
    #[serde(deserialize_with = "null_default")]
    pub tags: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub downloads: u64,
    pub rating: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub ratings: u64,
    #[serde(deserialize_with = "null_default")]
    pub updated: String,
    #[serde(deserialize_with = "null_default")]
    pub ds_status: String,
    /// Library only.
    pub latest: Option<LatestRef>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct LatestRef {
    pub id: i64,
    #[serde(deserialize_with = "null_default")]
    pub version: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ListPage {
    pub total: u64,
    pub page: u32,
    #[serde(rename = "pageCount")]
    pub page_count: u32,
    #[serde(deserialize_with = "null_default")]
    pub mods: Vec<ModSummary>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModDetail {
    #[serde(deserialize_with = "null_default")]
    pub slug: String,
    #[serde(deserialize_with = "null_default")]
    pub title: String,
    #[serde(deserialize_with = "null_default")]
    pub description: String,
    #[serde(deserialize_with = "null_default")]
    pub category: String,
    #[serde(deserialize_with = "null_default")]
    pub author: String,
    #[serde(deserialize_with = "null_default")]
    pub tags: Vec<String>,
    pub license: Option<String>,
    #[serde(rename = "gameVersion")]
    pub game_version: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub downloads: u64,
    pub rating: Option<f64>,
    #[serde(deserialize_with = "null_default")]
    pub ratings: u64,
    #[serde(deserialize_with = "null_default")]
    pub updated: String,
    #[serde(deserialize_with = "null_default")]
    pub platforms: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub ds_status: String,
    pub ds_verified: Option<DsVerified>,
    /// Newest first (by version number).
    #[serde(deserialize_with = "null_default")]
    pub versions: Vec<Version>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DsVerified {
    #[serde(deserialize_with = "null_default")]
    pub at: String,
    #[serde(deserialize_with = "null_default")]
    pub by: String,
    #[serde(deserialize_with = "null_default")]
    pub note: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Version {
    pub id: i64,
    #[serde(deserialize_with = "null_default")]
    pub version: String,
    #[serde(deserialize_with = "null_default")]
    pub size: u64,
    #[serde(deserialize_with = "null_default")]
    pub sha256: String,
    #[serde(deserialize_with = "null_default")]
    pub changelog: String,
    #[serde(deserialize_with = "null_default")]
    pub depends: Vec<String>,
    #[serde(deserialize_with = "null_default")]
    pub conflicts: Vec<String>,
    #[serde(rename = "manifestId")]
    pub manifest_id: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub platforms: Vec<String>,
    pub ds: Option<DsCheck>,
    #[serde(deserialize_with = "null_default")]
    pub downloads: u64,
    #[serde(deserialize_with = "null_default")]
    pub created: String,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DsCheck {
    pub compatible: bool,
    #[serde(deserialize_with = "null_default")]
    pub reasons: Vec<String>,
}
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct User {
    #[serde(deserialize_with = "null_default")]
    pub username: String,
    #[serde(deserialize_with = "null_default")]
    pub role: String,
    pub profile: Option<String>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct DeviceStart {
    pub code: String,
    pub verify_url: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}
impl std::fmt::Debug for DeviceStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceStart").field("code", &self.code).field("verify_url", &self.verify_url).finish_non_exhaustive()
    }
}
pub enum Poll {
    Pending,
    SlowDown(u64),
    Approved(Account),
}

/// Site categories (`?category=`) and their labels.
pub const CATEGORIES: [(&str, &str); 8] = [
    ("character", "Character"),
    ("track", "Track"),
    ("kart", "Kart"),
    ("battle_stage", "Battle stage"),
    ("music", "Music"),
    ("texture_pack", "Texture pack"),
    ("gameplay", "Gameplay"),
    ("other", "Other"),
];
pub const SORTS: [(&str, &str); 7] = [
    ("newest", "Newest"),
    ("updated", "Recently updated"),
    ("downloads", "Most downloaded"),
    ("downloads7d", "Popular this week"),
    ("rating", "Top rated"),
    ("reviews", "Most reviewed"),
    ("name", "Name"),
];
/// The package `type` (see PLUGIN_FORMAT.md) for a site category, for colouring.
pub fn kind_for_category(category: &str) -> &str {
    match category {
        "battle_stage" => "battle",
        "texture_pack" => "texture",
        c => c,
    }
}

/// What the browse view asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub q: String,
    pub category: String,
    pub sort: String,
    pub ds_only: bool,
    pub page: u32,
}
impl Default for Query {
    fn default() -> Self {
        Self { q: String::new(), category: String::new(), sort: "newest".into(), ds_only: false, page: 1 }
    }
}
impl Query {
    pub fn path(&self) -> String {
        let mut p = format!("/api/v1/mods?page={}", self.page.max(1));
        for (k, v) in [("q", self.q.trim()), ("category", &self.category), ("sort", &self.sort)] {
            if !v.is_empty() {
                p += &format!("&{k}={}", encode(v));
            }
        }
        if self.ds_only {
            p += "&ds=1";
        }
        p
    }
}
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
/// Slugs are lowercase words joined by `-`; anything else is refused before it becomes part of a URL.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.len() <= 100 && slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: u16,
    /// The site's `error` code, or `network` / `bad_response`.
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl ApiError {
    fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into() }
    }
    /// The token was revoked or is unknown: sign in again.
    pub fn signed_out(&self) -> bool {
        self.status == 401
    }
    /// From an error response body `{"error": "<code>"}`.
    pub fn from_response(status: u16, body: &[u8]) -> Self {
        let code = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_default();
        let message = match code.as_str() {
            "invalid_token" => "Your mkdsrust.com sign-in is no longer valid (revoked or banned). Sign in again.".into(),
            "missing_token" => "Sign in to mkdsrust.com to do this.".into(),
            "rate_limited" => "mkdsrust.com says: too many requests. Wait a few minutes and try again.".into(),
            "not_found" => "That mod is not on mkdsrust.com (removed or hidden).".into(),
            "file_missing" => "mkdsrust.com could not find the file for this version. Try another version.".into(),
            "" if status >= 500 => format!("mkdsrust.com had a problem (HTTP {status}). Try again later."),
            "" => format!("Unexpected answer from mkdsrust.com (HTTP {status})."),
            c => format!("mkdsrust.com refused the request ({c})."),
        };
        let code = if code.is_empty() { "bad_response".into() } else { code };
        Self { status, code, message }
    }
}
pub type ApiResult<T> = Result<T, ApiError>;

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::new(200, "bad_response", format!("Unexpected answer from mkdsrust.com: {e}")))
}

/// A quoted value for a curl config file (`-K`).
fn config_value(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
/// Split curl's stdout (`body` + `\n` + `%{http_code}`).
fn split_status(stdout: &[u8]) -> Option<(u16, &[u8])> {
    let at = stdout.iter().rposition(|&b| b == b'\n')?;
    let status = std::str::from_utf8(&stdout[at + 1..]).ok()?.trim().parse().ok()?;
    Some((status, &stdout[..at]))
}

#[derive(Clone)]
pub struct Client {
    pub base: String,
    token: Option<String>,
}
impl Client {
    pub fn new(token: Option<&str>) -> Self {
        Self { base: base(), token: token.map(str::to_string) }
    }
    fn curl(&self, method: &str, path: &str, body: Option<&serde_json::Value>, out: Option<&Path>, timeout: u64) -> ApiResult<(u16, Vec<u8>)> {
        let url = format!("{}{path}", self.base);
        let mut config = String::new();
        if let Some(t) = &self.token {
            config += &format!("header = {}\n", config_value(&format!("Authorization: Bearer {t}")));
        }
        if let Some(b) = body {
            config += "header = \"Content-Type: application/json\"\n";
            config += &format!("data-binary = {}\n", config_value(&b.to_string()));
        }
        let mut c = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
        crate::protocol::hidden(&mut c);
        c.args(["-sS", "-X", method, "--connect-timeout", "15", "--max-time", &timeout.to_string()])
            .args(["--proto", if url.starts_with("http://") { "=http,https" } else { "=https" }])
            .args(["--max-filesize", &mkds_builder::modpkg::MAX_ZIP.to_string()])
            .args(["-A", concat!("MKDSrust-launcher/", env!("CARGO_PKG_VERSION"))])
            .args(["-K", "-", "-w", "\\n%{http_code}"]);
        if let Some(o) = out {
            c.arg("-o").arg(o);
        }
        let mut child = c
            .arg(&url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ApiError::new(0, "network", format!("Could not start curl.exe (needs Windows 10 or later): {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(config.as_bytes());
        }
        let output = child.wait_with_output().map_err(|e| ApiError::new(0, "network", e.to_string()))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            let detail = err.trim().trim_start_matches("curl: ");
            let message = match output.status.code() {
                Some(28) => "mkdsrust.com did not answer in time. Check your connection and try again.".into(),
                Some(63) => "The download is larger than the 100 MB mod limit.".into(),
                Some(6 | 7) => format!("Could not reach mkdsrust.com. Check your internet connection. ({detail})"),
                _ => format!("Could not reach mkdsrust.com: {detail}"),
            };
            return Err(ApiError::new(0, "network", message));
        }
        let (status, body) = split_status(&output.stdout).ok_or_else(|| ApiError::new(0, "bad_response", "Unexpected answer from mkdsrust.com."))?;
        Ok((status, body.to_vec()))
    }
    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> ApiResult<T> {
        let (status, body) = self.curl("GET", path, None, None, API_TIMEOUT)?;
        if status != 200 {
            return Err(ApiError::from_response(status, &body));
        }
        parse(&body)
    }
    pub fn list(&self, q: &Query) -> ApiResult<ListPage> {
        self.get_json(&q.path())
    }
    pub fn detail(&self, slug: &str) -> ApiResult<ModDetail> {
        if !valid_slug(slug) {
            return Err(ApiError::new(0, "not_found", "Invalid mod name."));
        }
        self.get_json(&format!("/api/v1/mods/{slug}"))
    }
    pub fn me(&self) -> ApiResult<User> {
        self.get_json("/api/v1/me")
    }
    pub fn library(&self) -> ApiResult<Vec<ModSummary>> {
        #[derive(Deserialize)]
        struct Library {
            mods: Vec<ModSummary>,
        }
        Ok(self.get_json::<Library>("/api/v1/me/library")?.mods)
    }
    pub fn set_saved(&self, slug: &str, saved: bool) -> ApiResult<bool> {
        if !valid_slug(slug) {
            return Err(ApiError::new(0, "not_found", "Invalid mod name."));
        }
        let (status, body) = self.curl(if saved { "PUT" } else { "DELETE" }, &format!("/api/v1/me/library/{slug}"), None, None, API_TIMEOUT)?;
        if status != 200 {
            return Err(ApiError::from_response(status, &body));
        }
        Ok(parse::<serde_json::Value>(&body)?["saved"].as_bool().unwrap_or(saved))
    }
    pub fn device_start(&self, client_name: &str) -> ApiResult<DeviceStart> {
        let (status, body) = self.curl("POST", "/api/v1/device/start", Some(&serde_json::json!({"client_name": client_name})), None, API_TIMEOUT)?;
        if status != 200 {
            return Err(ApiError::from_response(status, &body));
        }
        let start: DeviceStart = parse(&body)?;
        if start.code.is_empty() || start.device_code.is_empty() {
            return Err(ApiError::new(status, "bad_response", "Unexpected answer from mkdsrust.com."));
        }
        Ok(start)
    }
    pub fn device_poll(&self, device_code: &str) -> ApiResult<Poll> {
        let (status, body) = self.curl("POST", "/api/v1/device/poll", Some(&serde_json::json!({"device_code": device_code})), None, API_TIMEOUT)?;
        poll_outcome(status, &body)
    }
    /// Download a version's zip to `dest` and check it against the version's SHA-256.
    pub fn download(&self, slug: &str, version: &Version, dest: &Path) -> ApiResult<()> {
        if !valid_slug(slug) {
            return Err(ApiError::new(0, "not_found", "Invalid mod name."));
        }
        if self.token.is_none() {
            return Err(ApiError::new(401, "missing_token", "Sign in to mkdsrust.com to download mods."));
        }
        let path = format!("/api/v1/mods/{slug}/versions/{}/download", version.id);
        let (status, _) = self.curl("GET", &path, None, Some(dest), DOWNLOAD_TIMEOUT)?;
        let bytes = std::fs::read(dest).map_err(|e| ApiError::new(status, "io", format!("Could not read the download: {e}")))?;
        if status != 200 {
            return Err(ApiError::from_response(status, &bytes));
        }
        check_download(&bytes, &version.sha256).map_err(|m| ApiError::new(status, "bad_download", m))
    }
}

pub fn check_download(bytes: &[u8], sha256: &str) -> Result<(), String> {
    if bytes.get(..2) != Some(b"PK") {
        return Err("mkdsrust.com did not send a mod package.".into());
    }
    if !sha256.is_empty() && !mkds_builder::modpkg::hash(bytes).eq_ignore_ascii_case(sha256) {
        return Err("The download is damaged (SHA-256 does not match the site). Try again.".into());
    }
    Ok(())
}

pub fn poll_outcome(status: u16, body: &[u8]) -> ApiResult<Poll> {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    if status == 200 {
        let token = v["token"].as_str().unwrap_or_default();
        if !valid_token(token) {
            return Err(ApiError::new(status, "bad_response", "Unexpected answer from mkdsrust.com."));
        }
        let user: User = serde_json::from_value(v["user"].clone()).unwrap_or_default();
        return Ok(Poll::Approved(Account { token: token.into(), username: user.username }));
    }
    match v["error"].as_str().unwrap_or_default() {
        "authorization_pending" => Ok(Poll::Pending),
        "slow_down" => Ok(Poll::SlowDown(v["interval"].as_u64().unwrap_or(10))),
        "access_denied" => Err(ApiError::new(status, "access_denied", "Sign-in was denied on mkdsrust.com.")),
        "expired_token" => Err(ApiError::new(status, "expired_token", "The sign-in code expired. Start again.")),
        "invalid_grant" => Err(ApiError::new(status, "invalid_grant", "The sign-in code is no longer valid. Start again.")),
        _ => Err(ApiError::from_response(status, body)),
    }
}

/// The name shown on the approval page and the account's token list (printable ASCII, ≤ 60).
pub fn client_name() -> String {
    let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    let name = if host.is_empty() { "MKDSrust launcher".to_string() } else { format!("MKDSrust launcher on {host}") };
    name.chars().filter(|c| (' '..='~').contains(c)).take(60).collect()
}

// ---- account (token) storage -------------------------------------------------------------------------------------

pub fn valid_token(t: &str) -> bool {
    t.len() > 4 && t.len() < 200 && t.starts_with("mkr_") && t[4..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub token: String,
    #[serde(default)]
    pub username: String,
}
impl std::fmt::Debug for Account {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account").field("username", &self.username).field("token", &"<hidden>").finish()
    }
}
pub fn load_account(dir: &Path) -> Option<Account> {
    let a: Account = serde_json::from_slice(&std::fs::read(dir.join(ACCOUNT_FILE)).ok()?).ok()?;
    valid_token(&a.token).then_some(a)
}
pub fn save_account(dir: &Path, account: &Account) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut f = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    f.write_all(&serde_json::to_vec_pretty(account).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    f.persist(dir.join(ACCOUNT_FILE)).map_err(|e| format!("Could not save the sign-in: {}", e.error))?;
    Ok(())
}
pub fn delete_account(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(dir.join(ACCOUNT_FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Could not remove the sign-in: {e}")),
        _ => Ok(()),
    }
}

// ---- installed mods and updates ----------------------------------------------------------------------------------

/// Where an installed mod came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub slug: String,
    pub version_id: i64,
    pub version: String,
}
pub fn write_source(game: &Path, id: &str, source: &Source) -> Result<(), String> {
    if !mkds_builder::modpkg::valid_id(id) {
        return Err("Invalid mod id".into());
    }
    let dir = game.join("mods").join(id);
    if !dir.join("manifest.json").is_file() {
        return Err(format!("{id} is not installed"));
    }
    std::fs::write(dir.join(SOURCE_FILE), serde_json::to_vec_pretty(source).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}
/// An installed mod (manifest id and version) and its recorded site source, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    pub id: String,
    pub version: String,
    pub source: Option<Source>,
}
pub fn installed(game: &Path) -> Vec<Installed> {
    mkds_builder::modpkg::installed(game)
        .into_iter()
        .map(|m| {
            let source = std::fs::read(game.join("mods").join(&m.id).join(SOURCE_FILE))
                .ok()
                .and_then(|b| serde_json::from_slice::<Source>(&b).ok())
                .filter(|s| valid_slug(&s.slug));
            Installed { id: m.id, version: m.version, source }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    NotInstalled,
    Installed { id: String, version: String },
    Update { id: String, installed: String, latest: String },
}
impl State {
    pub fn id(&self) -> Option<&str> {
        match self {
            State::NotInstalled => None,
            State::Installed { id, .. } | State::Update { id, .. } => Some(id),
        }
    }
}
/// The newest version the site knows of, as far as this view has it.
#[derive(Clone, Debug, Default)]
pub struct Latest<'a> {
    pub id: i64,
    pub version: &'a str,
    pub manifest_id: Option<&'a str>,
}
impl<'a> From<&'a Version> for Latest<'a> {
    fn from(v: &'a Version) -> Self {
        Self { id: v.id, version: &v.version, manifest_id: v.manifest_id.as_deref() }
    }
}
impl<'a> From<&'a LatestRef> for Latest<'a> {
    fn from(v: &'a LatestRef) -> Self {
        Self { id: v.id, version: &v.version, manifest_id: None }
    }
}
fn newer(latest: &str, installed: &str) -> Option<bool> {
    let l = semver::Version::parse(latest.trim_start_matches('v')).ok()?;
    let i = semver::Version::parse(installed.trim_start_matches('v')).ok()?;
    Some(l > i)
}
/// Is the site mod `slug` installed (by its recorded source, else by the latest version's manifest id), and is
/// there a newer version?
pub fn state(slug: &str, latest: Option<Latest>, installed: &[Installed]) -> State {
    let found = installed
        .iter()
        .find(|m| m.source.as_ref().is_some_and(|s| s.slug == slug))
        .or_else(|| latest.as_ref().and_then(|l| l.manifest_id).and_then(|mid| installed.iter().find(|m| m.id == mid)));
    let Some(m) = found else { return State::NotInstalled };
    let update = latest.as_ref().is_some_and(|l| {
        newer(l.version, &m.version).unwrap_or_else(|| match &m.source {
            Some(s) => s.version_id != l.id && s.version != l.version,
            None => l.version != m.version,
        })
    });
    if update {
        State::Update { id: m.id.clone(), installed: m.version.clone(), latest: latest.unwrap().version.to_string() }
    } else {
        State::Installed { id: m.id.clone(), version: m.version.clone() }
    }
}
/// Slugs of installed mods that came from the site (to check them for updates).
pub fn sourced_slugs(installed: &[Installed]) -> BTreeMap<String, String> {
    installed.iter().filter_map(|m| m.source.as_ref().map(|s| (s.slug.clone(), m.id.clone()))).collect()
}

/// Download `slug` (the given version, else the newest) and install it through the normal screening. Returns the
/// summary line.
pub fn install(client: &Client, slug: &str, version_id: Option<i64>, data: &Path, game: &Path, rom: &Path) -> ApiResult<String> {
    let io = |e: std::io::Error| ApiError::new(0, "io", e.to_string());
    let detail = client.detail(slug)?;
    let version = match version_id {
        Some(id) => detail.versions.iter().find(|v| v.id == id),
        None => detail.versions.first(),
    }
    .ok_or_else(|| ApiError::new(0, "not_found", "This mod has no downloadable version yet."))?;
    let dir = data.join("downloads");
    std::fs::create_dir_all(&dir).map_err(io)?;
    let file = tempfile::Builder::new().prefix("site-").suffix(".zip").tempfile_in(&dir).map_err(io)?;
    client.download(slug, version, file.path())?;
    let before: Vec<String> = mkds_builder::modpkg::installed(game).into_iter().map(|m| m.id).collect();
    let summary = crate::mods_ui::screen_and_install(game, rom, file.path()).map_err(|e| ApiError::new(0, "screening", format!("Mod not installed: {e}")))?;
    let after = mkds_builder::modpkg::installed(game);
    // the installed id: the site's copy of the manifest id, else whatever is new
    let id = version
        .manifest_id
        .clone()
        .filter(|id| after.iter().any(|m| &m.id == id))
        .or_else(|| after.iter().map(|m| m.id.clone()).find(|id| !before.contains(id)));
    if let Some(id) = id {
        let source = Source { slug: slug.into(), version_id: version.id, version: version.version.clone() };
        if let Err(e) = write_source(game, &id, &source) {
            return Ok(format!("{summary} (could not record its source for updates: {e})"));
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn works_on_ds(ds_status: &str) -> bool {
        matches!(ds_status, "ds_compatible" | "ds_verified")
    }

    #[test]
    fn parses_list_detail_and_library_json() {
        let list: ListPage = serde_json::from_str(
            r#"{"total": 6, "page": 1, "pageCount": 1, "mods": [{"slug": "retro-castle", "title": "Retro Castle", "category": "track",
            "author": "erin", "tags": ["castle"], "downloads": 4, "rating": 4.75, "ratings": 4, "updated": "2026-11-01T00:00:00Z",
            "ds_status": "pc_only"}, {"slug": "x", "title": "X", "category": "battle_stage", "author": "a", "tags": null,
            "downloads": 0, "rating": null, "ratings": 0, "updated": "u", "ds_status": "ds_verified", "future_field": 1}]}"#,
        )
        .unwrap();
        assert_eq!((list.total, list.page_count, list.mods.len()), (6, 1, 2));
        assert_eq!(list.mods[0].rating, Some(4.75));
        assert!(list.mods[1].rating.is_none() && list.mods[1].tags.is_empty() && works_on_ds(&list.mods[1].ds_status));
        assert_eq!(kind_for_category(&list.mods[1].category), "battle");
        let empty: ListPage = serde_json::from_str(r#"{"total":0,"page":1,"pageCount":1,"mods":[]}"#).unwrap();
        assert!(empty.mods.is_empty());

        let d: ModDetail = serde_json::from_str(
            r#"{"slug": "retro-castle", "title": "T", "description": null, "category": "track", "author": "erin", "tags": [],
            "license": "CC-BY-4.0", "gameVersion": "0.1", "downloads": 5, "rating": 4.75, "ratings": 4, "updated": "u",
            "platforms": ["pc", "ds"], "ds_status": "ds_verified", "ds_verified": {"at": "a", "by": "erin", "note": "n"},
            "versions": [{"id": 5, "version": "2.0.0", "size": 653, "sha256": "817c", "changelog": null,
            "depends": ["other.mod@>=1.0.0"], "conflicts": [], "manifestId": "erin.retro-castle", "platforms": ["pc", "ds"],
            "ds": {"compatible": true, "reasons": [], "romEstimate": 33554432}, "downloads": 3, "created": "c",
            "download": "/api/v1/mods/retro-castle/versions/5/download"},
            {"id": 2, "version": "1.0.0", "size": 1, "sha256": "aa", "changelog": "first", "manifestId": null, "ds": null}]}"#,
        )
        .unwrap();
        assert_eq!(d.description, "");
        assert_eq!(d.versions[0].manifest_id.as_deref(), Some("erin.retro-castle"));
        assert!(d.versions[0].ds.as_ref().unwrap().compatible && d.versions[1].ds.is_none());
        assert_eq!(d.versions[1].changelog, "first");
        assert_eq!(d.ds_verified.unwrap().by, "erin");

        let lib: ModSummary = serde_json::from_str(
            r#"{"slug": "s", "title": "S", "category": "music", "author": "a", "tags": [], "downloads": 1, "rating": null,
            "ratings": 0, "updated": "u", "saved": "v", "ds_status": "pc_only", "latest": {"id": 9, "version": "1.2.0", "download": "/x"}}"#,
        )
        .unwrap();
        assert_eq!(lib.latest.unwrap().id, 9);
        let user: User = serde_json::from_str(r#"{"username": "cam", "role": "member", "avatar": null, "profile": "p"}"#).unwrap();
        assert_eq!(user.username, "cam");
    }

    #[test]
    fn errors_and_device_poll() {
        let e = ApiError::from_response(401, br#"{"error":"invalid_token"}"#);
        assert!(e.signed_out() && e.code == "invalid_token" && e.message.contains("Sign in again"));
        assert_eq!(ApiError::from_response(502, b"<html>").code, "bad_response");
        assert!(ApiError::from_response(502, b"<html>").message.contains("502"));
        assert!(matches!(poll_outcome(400, br#"{"error":"authorization_pending"}"#), Ok(Poll::Pending)));
        assert!(matches!(poll_outcome(400, br#"{"error":"slow_down","interval":10}"#), Ok(Poll::SlowDown(10))));
        assert_eq!(poll_outcome(400, br#"{"error":"access_denied"}"#).err().unwrap().code, "access_denied");
        assert_eq!(poll_outcome(400, br#"{"error":"expired_token"}"#).err().unwrap().code, "expired_token");
        let token = format!("mkr_{}", "A".repeat(43));
        let ok = format!(r#"{{"token":"{token}","token_type":"Bearer","user":{{"username":"cam","role":"member"}}}}"#);
        match poll_outcome(200, ok.as_bytes()) {
            Ok(Poll::Approved(a)) => assert_eq!((a.token.as_str(), a.username.as_str()), (token.as_str(), "cam")),
            _ => panic!("approved"),
        }
        assert!(poll_outcome(200, br#"{"token":"nope"}"#).is_err());
        assert_eq!(split_status(b"{\"a\":1}\n200"), Some((200, b"{\"a\":1}".as_slice())));
        assert_eq!(split_status(b"\n404"), Some((404, b"".as_slice())));
        assert_eq!(config_value(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn query_strings_are_encoded() {
        let q = Query { q: "bowser castle&x".into(), category: "track".into(), sort: "rating".into(), ds_only: true, page: 2 };
        assert_eq!(q.path(), "/api/v1/mods?page=2&q=bowser%20castle%26x&category=track&sort=rating&ds=1");
        assert_eq!(Query::default().path(), "/api/v1/mods?page=1&sort=newest");
        assert!(valid_slug("retro-bowser-castle") && !valid_slug("../x") && !valid_slug("a/b") && !valid_slug(""));
    }

    #[test]
    fn token_storage_round_trips_and_hides_the_token() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_account(dir.path()).is_none());
        let a = Account { token: format!("mkr_{}", "x_Y-9".repeat(8)), username: "cam".into() };
        save_account(dir.path(), &a).unwrap();
        assert_eq!(load_account(dir.path()), Some(a.clone()));
        assert!(!format!("{a:?}").contains("mkr_"));
        std::fs::write(dir.path().join(ACCOUNT_FILE), r#"{"token":"not a token","username":"x"}"#).unwrap();
        assert!(load_account(dir.path()).is_none());
        save_account(dir.path(), &a).unwrap();
        delete_account(dir.path()).unwrap();
        delete_account(dir.path()).unwrap();
        assert!(load_account(dir.path()).is_none());
        assert!(!dir.path().join(ACCOUNT_FILE).exists());
    }

    #[test]
    fn update_detection() {
        let src = |slug: &str, id: i64, v: &str| Some(Source { slug: slug.into(), version_id: id, version: v.into() });
        let installed = vec![
            Installed { id: "erin.castle".into(), version: "1.0.0".into(), source: src("retro-castle", 2, "1.0.0") },
            Installed { id: "bob.music".into(), version: "2.0.0".into(), source: None },
            Installed { id: "odd.ver".into(), version: "beta".into(), source: src("odd", 7, "beta") },
        ];
        let latest = |id, version, mid| Some(Latest { id, version, manifest_id: mid });
        assert_eq!(state("nope", None, &installed), State::NotInstalled);
        // list rows (no version known): installed by recorded source
        assert_eq!(state("retro-castle", None, &installed), State::Installed { id: "erin.castle".into(), version: "1.0.0".into() });
        assert_eq!(
            state("retro-castle", latest(5, "2.0.0", Some("erin.castle")), &installed),
            State::Update { id: "erin.castle".into(), installed: "1.0.0".into(), latest: "2.0.0".into() }
        );
        assert_eq!(state("retro-castle", latest(2, "1.0.0", None), &installed).id(), Some("erin.castle"));
        assert!(matches!(state("retro-castle", latest(2, "1.0.0", None), &installed), State::Installed { .. }));
        // an older site version never counts as an update
        assert!(matches!(state("retro-castle", latest(1, "0.9.0", None), &installed), State::Installed { .. }));
        // installed from a zip or link (no source): matched through the manifest id
        assert!(matches!(state("music", latest(3, "2.1.0", Some("bob.music")), &installed), State::Update { .. }));
        assert!(matches!(state("music", latest(3, "2.0.0", Some("bob.music")), &installed), State::Installed { .. }));
        assert_eq!(state("music", latest(3, "2.1.0", None), &installed), State::NotInstalled);
        // versions that are not semver: a different version id and string is an update
        assert!(matches!(state("odd", latest(8, "gamma", None), &installed), State::Update { .. }));
        assert!(matches!(state("odd", latest(7, "beta", None), &installed), State::Installed { .. }));
        assert_eq!(sourced_slugs(&installed).len(), 2);
    }

    #[test]
    fn sources_are_recorded_beside_installed_mods() {
        let game = tempfile::tempdir().unwrap();
        let dir = game.path().join("mods/erin.castle");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = serde_json::json!({"format":1,"type":"track","id":"erin.castle","name":"Castle","version":"1.0.0",
            "author":"erin","game":"AMCE","files":{"new.bmg":{"add":"files/new.bmg"}}});
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        assert!(write_source(game.path(), "missing.mod", &Source::default()).is_err());
        let s = Source { slug: "retro-castle".into(), version_id: 5, version: "1.0.0".into() };
        write_source(game.path(), "erin.castle", &s).unwrap();
        let list = installed(game.path());
        assert_eq!(list, vec![Installed { id: "erin.castle".into(), version: "1.0.0".into(), source: Some(s) }]);
        // the mod still lists normally (the sidecar is not part of the strict manifest)
        assert_eq!(mkds_builder::modpkg::installed(game.path()).len(), 1);
    }

    #[test]
    fn downloads_are_checked() {
        let zip = b"PK\x03\x04rest";
        assert!(check_download(zip, &mkds_builder::modpkg::hash(zip)).is_ok());
        assert!(check_download(zip, &mkds_builder::modpkg::hash(zip).to_uppercase()).is_ok());
        assert!(check_download(zip, &"0".repeat(64)).unwrap_err().contains("SHA-256"));
        assert!(check_download(b"<html>", "").is_err());
    }

    #[test]
    #[ignore = "network: live mkdsrust.com"]
    fn live_listing_and_search() {
        let c = Client::new(None);
        let page = c.list(&Query::default()).unwrap();
        assert!(page.page_count >= 1 && page.mods.len() as u64 <= page.total.max(12));
        let none = c.list(&Query { q: "zzzz-no-such-mod-zzzz".into(), ..Default::default() }).unwrap();
        assert_eq!(none.total, 0);
        let ds = c.list(&Query { ds_only: true, sort: "rating".into(), category: "track".into(), ..Default::default() }).unwrap();
        assert!(ds.mods.iter().all(|m| works_on_ds(&m.ds_status)));
        assert!(c.me().unwrap_err().signed_out());
        assert_eq!(c.detail("zzzz-no-such-mod").unwrap_err().code, "not_found");
        // the example mods (live since 2026-11-02)
        for m in &page.mods {
            let d = c.detail(&m.slug).unwrap();
            assert!(!d.versions.is_empty() && d.versions[0].sha256.len() == 64, "{}", m.slug);
            assert!(!d.platforms.is_empty());
        }
    }
}
