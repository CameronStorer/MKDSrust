# MKDSrust: Tauri 2 tab/plugin architecture and reusable DS format libraries

Research date: 2026-11-11. Every external claim is followed by its source URL. Where a source was secondary
(blog, mirror, aggregator) it is marked *(secondary)*. Where I could not verify something it says so.

Local context checked (read-only): the Toolbox at `\\192.168.1.11\Shared\Coding\My_NDS_GAME\Toolbox\MKDSrust-Toolbox`
already uses `tauri = "2"`, `tauri-plugin-dialog`, `tauri-specta =2.0.0-rc.21`, and its `nitro-formats` crate
(about 6.7k lines) already has modules for `rom`, `rom_build`, `archive` (NARC), `lz77`, `blz`, `bmg`, `g3d`/`model`/
`texture`/`animation`, `kcl`, `nkm`, `graphics`/`cell`, `audio/{sdat,sequence,bank,wave,stream}` and `bps`.
`CREDITS.md` records that apicula, MKTB, ndspy, Tinke, EveryFileExplorer and NitroPaint were *studied*, with no code
copied. **So Part B is mostly "what can we use as a test oracle or for gaps", not "what should replace our code".**

---

## Part A: Tauri 2 architecture for on-demand tabs

### A1. Ways to load a downloaded frontend bundle

| Approach | How it works | Pros | Cons |
|---|---|---|---|
| **Asset protocol + `convertFileSrc` + `import()`** | Enable `app.security.assetProtocol` with a scope (e.g. `$APPDATA/components/**/*`); convert the plugin entry path with `convertFileSrc`; `await import(url)` and read the default export. | Built in, no server, works with ESM output from Vite. | Asset scope is static config (runtime scope possible, see below). CSP must allow `asset:` / `http://asset.localhost`. The plugin runs in the main page with full access to `window.__TAURI_INTERNALS__`. |
| **Custom URI scheme (`register_asynchronous_uri_scheme_protocol`)** | Rust serves `mkds-ext://toolbox/...` from the app data dir. You decide the MIME types, check hashes, and refuse paths outside the component folder. | Full control: integrity checks, path validation, correct `Content-Type` for `.js`/`.wasm`/`.glb`. Async, so large models don't block the main thread. | More code. On Windows WebView2 the scheme is rewritten to `https://<scheme>.localhost`, so CSP and origin checks must use that form. |
| **Module Federation (`@module-federation/vite`)** | Host loads a remote's `remoteEntry.js` at runtime and shares React and other singletons. | Shared React instance, so the bundle is smaller. Version negotiation. | Extra runtime and build complexity. ESM remotes must be declared `type: 'module'`. No Tauri-specific docs found; CORS/CSP over custom protocols is untested. |
| **Separate webview/window or sandboxed iframe** | The tab is its own webview (label `toolbox`) or an iframe, talking over postMessage RPC. | Strong isolation, and capabilities can be scoped per window label. | Iframe and host can't be told apart on Linux/Android for remote-URL capabilities. More memory. The Isolation pattern can't load external files in sandboxed iframes on Windows. |
| **`tauri-plugin-localhost`** | Serves assets over a localhost HTTP server. | Simple. | Its README warns of "considerable security risks" and recommends the default custom protocol. **Avoid.** |

Sources:
- Asset protocol + dynamic import walkthrough (Tauri v2), including the CSP additions `asset:` and `http://asset.localhost`, the `$APPCONFIG/**` scope, `convertFileSrc` + `import()` with a default export, and the note that plugins reach `__TAURI_INTERNALS__`. It also shows an optional QuickJS-in-WASM sandbox: https://marcos-brito.github.io/nnago/posts/tauri-and-plugins *(secondary, but concrete)*
- Asset protocol config: must set `enable: true` plus `scope`; allow/deny object form; deny wins; `$VAR` prefixes; avoid broad globs; `persisted-scope` plugin for runtime-picked paths: https://v2.tauri.app/security/asset-protocol/
- Runtime scope API `allow_directory(path, recursive)` on `Scopes`: https://docs.rs/crate/tauri/2.6.2/source/src/scope/mod.rs
- Async custom scheme registration added to avoid blocking the main thread: https://v2.tauri.app/release/tauri/v1.0.0-beta.8/ ; the handler receives `UriSchemeContext` (app handle + webview label) since 2.0.0-rc.17: https://v2.tauri.app/release/tauri/v2.0.0-rc.17
- Windows rewrites `app://localhost` to `https://app.localhost` in handlers: https://github-redirect.dependabot.com/tauri-apps/wry/issues/1129 *(mirror of the wry issue)*
- Module Federation for Vite (official plugin, `remoteEntry.js`, runtime-only host): https://github.com/module-federation/vite ; ESM remotes need `type: 'module'`: https://www.pkgpulse.com/guides/module-federation-2-webpack-rspack-vite-micro-frontends-2026 *(secondary)*
- `tauri-plugin-localhost` security warning: https://docs.rs/crate/tauri-plugin-localhost/2.3.2
- Remote-URL capabilities can't separate an iframe from its window on Linux/Android: https://v2.tauri.app/security/capabilities/
- Isolation pattern: intercepts IPC in a sandboxed iframe; on Windows external files don't load in sandboxed iframes, so scripts must be inlined: https://nhimg.org/tauri-security-guide *(secondary)*

### A2. Adding Rust backend functionality on demand

| Option | Fit for MKDSrust |
|---|---|
| **One backend binary with every lightweight crate compiled in (feature crates)** | Best for the Toolbox. `nitro-formats`, `mkds-data` and `toolbox-core` are pure Rust with few dependencies (thiserror, serde, png, zip...), so compiling them in costs little binary size. "Installing" the Toolbox tab then only downloads the **frontend bundle** (React + three.js, the heavy part) and possibly data. Commands can be registered always and gated by capabilities. |
| **Sidecar / child process** | Best for the **native-build toolchain** and **DS export toolchain**: large, separately versioned, crash-isolated, and possibly not Rust (compilers, linkers). Tauri's sidecar feature (`bundle.externalBin`, `-$TARGET_TRIPLE` suffix, `shell:allow-execute` with `"sidecar": true`) assumes binaries are **bundled at build time**; the docs don't cover downloaded sidecars. For downloaded tools, spawn them yourself from Rust with `std::process::Command`, using absolute paths inside the components dir after verifying their signature. Don't expose a generic shell to JS. Yaak is a real example: a Rust-managed Node plugin runtime talking over a local websocket. |
| **Dynamic library (`.dll`/`.so`/`.dylib`)** | Not recommended. Rust has no stable ABI. You'd need `extern "C"` / `#[repr(C)]` or `abi_stable`/`stabby`. `abi_stable` doesn't support unloading, and mismatched definitions cause silent wrong calls. You'd also need 3 OS builds per component and per host version, and Tauri commands can't be registered after `Builder::run` anyway. |
| **WASM (wasmtime) plugins** | An option for third-party, untrusted mods later; one Tauri/React app (Orbis) does this. Overkill for first-party tabs. |

Sources:
- Sidecar docs: https://v2.tauri.app/develop/sidecar/
- Yaak plugins run in a Node runtime: https://unpkg.com/@yaakapp/api@0.6.4/README.md ; logs show `yaak_plugins::nodejs` starting `plugin-runtime/index.cjs` over a websocket: https://yaakapp.featurebase.app/p/cant-build-on-macos
- Rust ABI instability and `abi_stable` limits (no unloading, load-time layout checks): https://crates-io.ganymede-static.progval.net/crates/abi_stable/0.11.2 ; `stabby`: https://docs.rs/crate/stabby/4.0.5/source/TUTORIAL.md ; compiler-version pinning warning: https://docs.rs/crate/dyn-loader/0.5.1
- WASM-sandboxed Tauri plugins (Orbis): https://dev.to/cyberpath/orbis-building-a-plugin-driven-desktop-platform-with-rust-and-react-4ikh *(secondary)*

### A3. Security: capabilities, runtime ACL and CSP

- Capabilities are files in `src-tauri/capabilities`, target windows by **label** (wildcards allowed), merge when several match, can be limited per platform, and a window with no matching capability has no IPC access. Capabilities **do not** protect against bad Rust code, overly broad scopes, WebView bugs or supply-chain compromise: https://v2.tauri.app/security/capabilities/ ; reference: https://v2.tauri.app/reference/acl/capability/
- **Runtime capabilities:** `Manager::add_capability` and `tauri::ipc::CapabilityBuilder`, behind the `dynamic-acl` feature, can add a capability when a component is installed. For example, grant `toolbox:*` commands only to the `main` webview after the Toolbox is installed and verified: https://docs.rs/tauri/latest/tauri/trait.Manager.html , https://docs.rs/tauri/2.11.1/tauri/ipc/struct.CapabilityBuilder.html
- `build.removeUnusedCommands` strips commands that are not allowed in the ACL, but it **ignores ACLs added at runtime**. If you use `add_capability`, keep the Toolbox commands referenced in a static capability or don't enable that option: https://v2.tauri.app/concept/size/
- CSP: only enforced if set in config. Tauri auto-adds hashes and nonces for local scripts and styles. Add `'wasm-unsafe-eval'` if any WASM runs. Avoid remote/CDN scripts. The example CSP allows `ipc:` / `http://ipc.localhost` for `connect-src`: https://v2.tauri.app/security/csp/
- **Integrity:** treat every downloaded component as code. Ship a signed manifest (same minisign/Ed25519 key family as the updater) that lists each file's SHA-256, and verify it before extracting or serving. The `minisign-verify` crate (the updater uses it) is zero-dependency and supports streaming/prehashed verification: https://docs.rs/crate/tauri-plugin-updater/2.10.1/source/src/updater.rs , https://www.meterian.io/components/rust/minisign-verify

### A4. Auto-update (`tauri-plugin-updater`)

- Signing is mandatory and can't be disabled. Works with a static JSON (e.g. GitHub Releases) or a dynamic server (204 means no update). Windows `installMode`: `passive` (default) / `basicUi` / `quiet`. `createUpdaterArtifacts: true`. Linux updates use the **AppImage** as the bundle. Endpoints can be set at runtime in Rust only (`updater_builder().endpoints(...)`), which allows channels such as stable/nightly. The updater does **not** download arbitrary extra files: https://v2.tauri.app/plugin/updater/
- **CVE-2026-95625** (published 2026-09-23): the update manifest was unsigned, so a crafted manifest could force a rollback to an older signed build. Fixed in **tauri-plugin-updater 2.12.0**, but only once you **enable `requireSignedVersion`**: https://app.opencve.io/cve/CVE-2026-95625 , https://security-tracker.debian.org/tracker/CVE-2026-95625
- Consequence: the updater covers the **launcher shell only**. Components (Toolbox tab, toolchains) need our own small downloader that uses the same signing approach. Apply the CVE lesson there too: sign the manifest including version and hashes, and refuse downgrades.

### A5. Packaging size and platform pitfalls

- **Windows:** WebView2 is preinstalled on Windows 10 1803+ and Windows 11: https://v2.tauri.app/start/prerequisites/ . Installer modes: `downloadBootstrapper` (default, +0 MB), `embedBootstrapper` (~+1.8 MB), `offlineInstaller` (~+127 MB), `fixedVersion` (~+150–180 MB): https://v1.tauri.app/v1/guides/building/windows (v1 numbers; v2 uses `webviewInstallMode`: https://v2.tauri.app/release/tauri/v2.8.3/ ) ; Microsoft's bootstrapper is about 2 MB: https://learn.microsoft.com/microsoft-edge/webview2/concepts/distribution . **Use the default `downloadBootstrapper`.**
- **Linux:** needs WebKitGTK 4.1. The AppImage grows "from the 2-6 MB range to 70+ MB". `bundleMediaFramework` adds GStreamer (needed only for audio/video playback in the webview). Build on the **oldest** supported distro (Ubuntu 22.04 / Debian 12) to avoid glibc errors. ARM AppImages can't be cross-compiled: https://v2.tauri.app/distribute/appimage/ . Ship a `.deb`/`.rpm` (small, uses system WebKitGTK) alongside the AppImage.
- **Size tuning:** `codegen-units = 1`, `lto = true`, `opt-level = "s"`, `panic = "abort"`, `strip = true`, `removeUnusedCommands`: https://v2.tauri.app/concept/size/
- **WebKitGTK pitfalls (important for the three.js course viewer):** blank/white windows, flicker and crashes on resize with NVIDIA, mostly from the DMABUF renderer. Ordered workarounds: `nvidia_drm.modeset=1`, `__NV_DISABLE_EXPLICIT_SYNC=1`, `WEBKIT_DISABLE_DMABUF_RENDERER=1`, and `WEBKIT_DISABLE_COMPOSITING_MODE=1` as a last resort. Don't apply these unconditionally. WebGL can silently fall onto a slow path, and `WEBGL_debug_renderer_info` reports "Apple GPU" on Linux. **Provide a non-WebGL fallback or a renderer setting on Linux**: https://v2.tauri.app/develop/debug/linux-graphics/ ; open WebKit WebGL perf bug: https://bugs.webkit.org/show_bug.cgi?id=278695 ; community reports that WebKitGTK is slower than Chromium: https://github-redirect.dependabot.com/tauri-apps/wry/discussions/1014 *(secondary)*
- **Startup time:** I found no authoritative benchmark, so measure it rather than assume. Practical steps: keep the launcher's first screen in the main bundle with no three.js; lazy-load the Toolbox (`React.lazy` + dynamic import) only when its tab opens; and avoid synchronous large IPC payloads at startup by passing binary data via the custom protocol (`ArrayBuffer` fetch) instead of JSON.
- **macOS:** `WKWebView` (system). No size cost, but notarization is needed for the updater and for downloaded executables (general Apple requirement, not verified here).

### A6. Real Tauri apps with extension systems

| App | Mechanism | Source |
|---|---|---|
| **Kunkun** (launcher, ~1.3k stars, WIP) | UI extensions in **iframes**, headless extensions in **web workers**. Typed RPC (`kkrpc`) and per-extension permissioned Tauri API access via `tauri-api-adapter`. | https://github.com/kunkunsh/kunkun ; https://dev.to/huakun/kkrpc-15m8 ; https://backiee.wasmer.app/http_github_com/kunkunsh/tauri-api-adapter |
| **Jan** (local AI) | Extensions built separately as `.tgz`, loaded at runtime. Its docs are inconsistent (an Electron-era page vs Tauri code). | https://git.biohazardvfx.com/Nicholai/jan/src/branch/mobile/init-mobile/CLAUDE.md *(mirror)*, https://jan.ai/docs/install-extensions |
| **Yaak** (API client) | TypeScript plugins in a Rust-managed Node runtime over a websocket. | links in A2 |
| **tauri-plugin-js** | Manages Bun/Node/Deno processes with typed RPC. | https://docs.rs/crate/tauri-plugin-js/0.1.0 |
| **Orbis** | WASM (wasmtime) sandboxed plugins + React. | link in A2 |
| Community list | | https://github.com/dubisdev/awesome-tauri |

None of these are in the same position as us (first-party, trusted tabs). Kunkun is the best reference if third-party mod plugins come later.

---

## Part B: Nintendo DS format libraries

Columns: URL | language | license | maintenance (as observed) | coverage | fit for MKDSrust (GPL-3.0).

### B1. ROM / NitroFS / NARC / CARC

| Project | Lang | License | Status | Covers | Fit |
|---|---|---|---|---|---|
| **ds-rom** (AetiasHax) https://github.com/AetiasHax/ds-rom , docs https://docs.rs/crate/ds-rom/latest | Rust | **MIT** | Active: v0.8.0 released 2026-07-21; 169 commits | Extract a ROM to files and **rebuild a matching ROM**; encrypted ROMs with a key; `compress`, `crc`, `crypto`, `rom` modules; `dsrom` CLI. DSiWare not yet supported. | **High.** It is the base of the `ds-decomp` toolkit (https://docs.rs/ds-decomp/0.8.0/ds_decomp), so it is decomp-grade. Use it as a dependency or at least as a byte-match oracle for our `rom_build.rs`. I could not confirm from the docs page whether `compress` includes BLZ (arm9/overlay backwards LZ); check `ds_rom::compress` before relying on it. |
| **nds / nitro_fs / narc** (Roughsketch) https://github.com/Roughsketch/nds | Rust | MIT | Stale: last crate 0.2.0 (2022-09-15) https://docs.rs/crate/narc/latest | NitroFS FAT/FNT and NARC reading | Low. We already have this. |
| **romkit** (platinvm) https://github.com/platinvm/romkit , https://docs.rs/crate/romkit/latest | Rust | MIT OR Apache-2.0 | New: 0.1.1 released 2026-10-15; 2 stars; ~41% documented | NitroFS, NARC, SDAT→MIDI/WAV, LZ77, NCLR/NCGR/NCER/NSCR, NSBMD/NSBTX; read-only; Pokémon-focused | Low or medium. Useful as a second opinion only. Too young to depend on. |
| **NitroPacker** (haroohie-club) https://github.com/Ermelber (pinned) | C# | not verified | — | ROM unpack/repack, ARM9/overlay patching | Reference only. |
| **NitroEdit** (XorTroll) https://github.com/XorTroll/NitroEdit | C++ (on-console) | MIT | Proof of concept | ROM header/icon, NitroFS, NARC/CARC, NCGR/NCLR/NSCR, SWAR/SWAV, BMG | Reference only. |
| **nitro-fs** (DanielPXL, npm) https://github-three-lyart.vercel.app/DanielPXL/nitro-fs *(mirror)*, https://deploy-preview-1210--yarnpkg.netlify.app/en/package/nitro-fs | TypeScript | **LGPL-3.0-or-later** | WIP, v1.1.1 | NitroFS read, SDAT (SSEQ/STRM/SWAR/SBNK/SSAR), BTX | Low. Our Rust side already does this, and we don't want parsing in JS. |

### B2. Compression (LZ10/LZ11/BLZ)

| Project | Lang | License | Status | Covers | Fit |
|---|---|---|---|---|---|
| **nintendo-lz** https://lib.rs/crates/nintendo-lz (GitLab darkkirb) | Rust (2015 edition) | BSD-2-Clause | Stale: 0.1.3 released 2019-03-01 | LZ10/LZ11 compress and decompress | Low. We have `lz77.rs`. Possibly useful as a fuzz oracle. |
| **ds-rom `compress`** | Rust | MIT | Active | Compression used for ROM building (exact algorithms unverified) | **Check it for BLZ.** If it matches, it is a good oracle for our `blz.rs`. |
| **AuroraLib.Compression** https://www.nuget.org/packages/AuroraLib.Compression.Nintendo | C# | not verified | Active | LZ10/LZ11/LZ40, 3DS BLZ | Reference only. |
| `blz-nx` https://docs.rs/crate/blz-nx/1.0.1/source/ | Rust | — | — | **Switch** BLZ, not DS | Not applicable. |

### B3. 3D: NSBMD / NSBTX / NSBCA / NSBTP / NSBTA

| Project | Lang | License | Status | Covers | Fit |
|---|---|---|---|---|---|
| **apicula** (scurest) https://github.com/scurest/apicula | Rust | **0BSD** (no attribution needed, GPL-compatible) | 255 commits, 233 stars, 18 open issues; no tagged releases; auto Windows build from master | NSBMD, NSBTX, NSBCA, NSBTP, NSBTA (experimental) → COLLADA `.dae` / glTF `.glb`; `extract`/`view`/`convert`/`info`. Pattern/material animations work in the viewer but not in convert. | **High as a reference and oracle.** It is a **CLI, not a library crate**, so you can't depend on it via crates.io; copy modules instead, which 0BSD permits with no conditions. Our `g3d.rs` already uses it as a reference. Best use: golden-file tests comparing our glTF export against `apicula convert` output on the user's own ROM (never committed). |
| **noclip.website** `src/nns_g3d` https://github.com/magcius/noclip.website/tree/main/src/nns_g3d | TypeScript (WebGL) | **MIT** (with a note that parts were reverse-engineered, not clean-room) https://raw.githubusercontent.com/magcius/noclip.website/main/LICENSE | Active project | `NNS_G3D.ts` parses NSBMD/NSBTX/NSBTA/NSBTP. `Scenes_MarioKartDS.ts` loads `Course/<id>.carc` + `<id>Tex.carc`, `course_model(.nsbmd/.nsbtx/.nsbta/.nsbtp)`, the `_V` skybox, `MapObj.carc`, and parses NKM `OBJI` (0x3C records) to spawn objects. No KCL. https://github.com/magcius/noclip.website/blob/main/src/nns_g3d/Scenes_MarioKartDS.ts | **High as a reference** for the in-webview MKDS course viewer (DS material/GX emulation in shaders, texture SRT/pattern animation). It uses its own gfx layer rather than three.js, so port ideas, not code. |
| **mkjs** https://github.junw03049948.workers.dev/oRiamn/mkjs *(mirror)* | TypeScript | not verified | Experimental | JS reimplementation of MKDS | Reference only. |

### B4. Audio: SDAT / SSEQ / SBNK / SWAR / STRM

| Project | Lang | License | Status | Covers | Fit |
|---|---|---|---|---|---|
| **ndspy** (RoadrunnerWMC) https://github.com/RoadrunnerWMC/ndspy , docs https://ndspy.readthedocs.io/en/stable/api/soundArchive.html | Python 3.8+ | **GPL-3.0** | 138 commits; no tagged GitHub releases shown | ROM, NARC, LZ10, BMG, SDAT (SSEQ/SSAR/SBNK/SWAR/SWAV/STRM), code compression, textures | **Medium as an oracle.** Same license family as ours, so even porting code would be allowed. It is Python, so use it only in dev/CI tests (round-trip comparison), never shipped. The SDAT structure appendix is the clearest spec: https://ndspy.readthedocs.io/en/stable/appendices/sdat-structure.html |
| **VGMTrans** https://github.com/vgmtrans/vgmtrans | C++ | zlib (some bundled parts under their own licenses) | Active | SDAT → MIDI/SF2/DLS, many systems | Medium. External tool for verifying our SSEQ→MIDI and SBNK→SF2 conversions. |
| **sdatxtract** https://github.com/oreo639/sdatxtract | C | not verified | — | Dump SDATs | Low. |
| **SDATStuff** https://github.com/soneek/SDATStuff | C | not verified | — | NCSF creation | Low. |
| **SSEQPlayer** https://github.com/fincs/SSEQPlayer | C (DS) | not verified | Old | Accurate SSEQ playback reference | Reference for sequencer semantics. |
| **kotlinds** https://klibs.io/project/kotlinds/kotlinds | Kotlin | not verified | — | NARC/SDAT repack, STRM/SWAR→WAV, SSEQ→MIDI, SF2 | Low. |
| No dedicated Rust SDAT crate found besides romkit's read-only module. | | | | | |

### B5. Text: BMG

- No Rust BMG crate found. The nearest are `tomolib` (Switch MSBT/MSBP): https://docs.rs/tomolib , and `roead` (BYML/SARC/AAMP, no BMG confirmed): https://lib.rs/crates/roead
- References: ndspy BMG (GPL-3.0), NitroEdit BMG view/edit (MIT), MKDS Course Modifier (opens `.BMG`): https://fileinfo.com/software/mkds_course_modifier/mkds_course_modifier
- Fit: keep our `bmg.rs`. Use ndspy as the round-trip oracle.

### B6. MKDS-specific: NKM, KCL

| Project | Lang | License | Status | Covers | Fit |
|---|---|---|---|---|---|
| **Mario Kart Toolbox (MKTB)** HaroohiePals https://github.com/HaroohiePals/MarioKartToolbox | C# (.NET, ImGui/OpenTK) | **MIT** | **Active**: repo updated 2026-10-08 https://github.com/orgs/HaroohiePals/repositories ; 257 commits, 18 open issues | Visual editing of all NKM sections; KCL generation/viewer (v6.1.0 added a KCL calculator: https://nsmbhd.net/post/58794/ ); `G3dModelManager`, `MkdsBinaryCourse` | **The canonical reference** for NKM/KCL semantics (already credited). Port logic freely under MIT with attribution. It is the closest competitor, so our differentiator is cross-platform, native, and tied to the decomp. |
| **NitroEffectMaker** (HaroohiePals) | C# | MIT | updated 2025-08-22 | SPA particle editor | Reference if we add particle editing. |
| **MKDS-decomp-headers** (HaroohiePals) | C | not shown | 2023 | Game structs from the Gericom/Ermelber decomp | Cross-check structure names in our decomp. |
| **mkds** (PyPI, "marIOkart" docs) https://blayyyyyk.github.io/marIOkart/api/mkds/nkm.html , https://blayyyyyk.github.io/marIOkart/_sources/api/articles/READING_FILES.md.txt | Python | not stated | WIP; flags its own inconsistencies (PATH loop flag) | NKM (OBJI, PATH, POIT, STAG, KTPS, KTPJ, KTP2...; header offsets +0x4C) and KCL (prism octree) | Low or medium. Useful as a test cross-check, but less trustworthy than MKTB. |
| **mkds-re** (XorTroll) https://github.com/XorTroll/mkds-re | C++/Python | MIT | 29 commits | EU (AMCP) RE, save editor, `mkdsutil` | Reference for save format, not courses. |
| **KCollisionLibrary** (KillzXGaming) https://www.github.com/KillzXGaming/KCollisionLibrary | C# | not verified | — | KCL read/write/hit-tests (Wii-era focus) | Reference for octree building. |
| **Wiimm KCL wiki** https://szs.wiimm.de/r/wiki/KCL | doc | — | — | KCL format (MKW, same family as MKDS) | Spec reference. |
| **MKDS Course Modifier** (Gericom) | C# | source status not verified | Old | NARC/BMG/NSBCA/SPA/NKM | Historical. |
| **TCRF NKM version notes** (0x1E / 0x24 / 0x25) https://tcrf.net/Mario_Kart_DS/Unused_Courses | doc | — | — | Prototype NKM versions | Useful if we load beta courses. |

No Rust NKM or KCL crate exists that I could find (the `kcl-lib` crate on crates.io is KittyCAD, unrelated: https://docs.rs/crate/kcl-lib/latest/source/src/). Our `nkm.rs`/`kcl.rs` appear to be the only Rust implementations.

### B7. 2D graphics (for completeness)

- **NitroPaint** https://github.com/Garhoogin/NitroPaint (C, BSD-2-Clause per our CREDITS.md). **Tinke** https://github.com/pleonex/tinke (GPL-3.0). **EveryFileExplorer** https://github.com/Gericom/EveryFileExplorer (license listed as "Mixed" on https://www.gamebrew.org/wiki/EveryFileExplorer_3DS , GPL-3.0 per our CREDITS.md; check before porting). DS Pokémon Hacking NCER/NANR docs: https://ds-pokemon-hacking.github.io/docs/universal/resources/nitro/graphics_2d/section_cebk/

---

## Recommendations

### Tab/plugin architecture (what to build)

1. **One Rust backend, components are data plus frontend.** Compile `nitro-formats`, `mkds-data` and `toolbox-core` into the launcher binary as workspace crates. They are small and pure Rust. What gets downloaded for the "Toolbox" component is its **frontend bundle** (React tab + three.js + assets) and any large data. Register Toolbox commands statically, and only **grant** them through a runtime capability (`dynamic-acl` → `add_capability`) once the component is installed and verified. Don't use dylib plugins.
2. **Serve installed components through our own async custom protocol** (`mkds-component://<id>/<file>`) from `$APPDATA/components/<id>/<version>/`. Rust validates paths, sets MIME types and serves only files whose hash matches the signed manifest. The frontend loads the tab entry with `React.lazy(() => import("mkds-component://toolbox/index.js"))`. On Windows, allow the `https://mkds-component.localhost` form in the CSP. Prefer this over the generic asset protocol, which is broader and statically scoped, and **never use `tauri-plugin-localhost`**.
3. **Sharing React:** start simple. Expose the host's `react`, `react-dom`, `zustand` and the typed IPC client on a frozen `window.__MKDS_HOST__` (or via an import map), and build the Toolbox bundle with those marked `external`. Adopt `@module-federation/vite` only if versioning pain appears.
4. **Toolchains (native build, DS export) are sidecar-style child processes**, downloaded into `$APPDATA/components/<id>/`, verified, and spawned by fixed Rust commands with fixed argument shapes. Never expose `shell:allow-execute` with free-form arguments to JS.
5. **One signing story:** the Tauri updater (≥ 2.12.0 with `requireSignedVersion: true`, per CVE-2026-95625) updates the shell. A small component manager uses `minisign-verify` with our own key over a **signed manifest** (id, version, min-host-version, file list + SHA-256), and refuses downgrades. Hosting can be GitHub Releases.
6. **Keep the first paint light:** the launcher home tab has no three.js. The Toolbox chunk loads only when its tab opens. Use `opt-level="s"`, LTO, strip, `panic=abort`. Use `downloadBootstrapper` for WebView2. On Linux, ship `.deb`/`.rpm` plus AppImage built on Ubuntu 22.04, and add a "Safe rendering" setting that sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` on next launch, plus a non-WebGL fallback view for the Toolbox.
7. **Later, for third-party mods:** follow Kunkun's model (sandboxed iframe or worker + typed RPC + per-extension permissions), or WASM. Keep it separate from first-party tabs.

### Format libraries (what to adopt)

| Need | Decision | License note |
|---|---|---|
| ROM extract/rebuild (matching) | **Evaluate `ds-rom` as a dependency**, or at minimum as a byte-for-byte oracle for `rom_build.rs`; check its `compress` module for BLZ | MIT, GPL-compatible; keep the notice |
| NSBMD/NSBTX/NSBCA/NSBTP/NSBTA | **Keep our `g3d.rs`.** Use **apicula** as the golden-output oracle in CI (on user-supplied ROMs) and copy modules where useful | 0BSD, no conditions |
| In-webview course rendering | Use **noclip.website `nns_g3d`** as the design reference for DS material/animation shading in three.js | MIT; attribute if code is ported; its author notes not all parts are clean-room |
| NKM / KCL | **Keep `nkm.rs`/`kcl.rs`.** Follow **MKTB** semantics; optionally cross-test against the `mkds` PyPI parser | MKTB MIT (attribute); `mkds` license unknown, so use it as a test tool only |
| SDAT family | **Keep `audio/*`.** Use **ndspy** round-trips and **VGMTrans** MIDI/SF2 output as dev-only oracles | ndspy GPL-3.0 (compatible with us); VGMTrans zlib |
| BMG | Keep `bmg.rs`. ndspy as oracle | — |
| LZ10/LZ11 | Keep `lz77.rs`. Optionally fuzz against `nintendo-lz` | BSD-2-Clause |
| Avoid as dependencies | `nitro_fs`/`narc` (stale), `romkit` (too young), `nitro-fs` npm (LGPL, JS-side), all C#/Python tools | — |

Bottom line: there is no mature Rust ecosystem to replace `nitro-formats`. The only Rust project worth depending on is
**ds-rom** (MIT, actively used for DS decomps). **apicula** (0BSD) and **MKTB** (MIT) are the best references, and
**ndspy**/**VGMTrans**/**noclip** are the best test oracles. All licenses listed are compatible with GPL-3.0 distribution.
