# MKDS modding community and tools: research report

Researched 2026-11-11 for MKDSrust (GPL-3.0). I found every claim below through web search and page fetches. I downloaded and ran no binaries.
Some pages were blocked or unreachable: GBAtemp (403), speedrun.com guides (403), the git.randommeaninglesscharacters.com mirrors (403), and dswiki.garhoogin.com (expired TLS certificate). Where a fact came only from a search-engine snippet or a third-party index, I say so.
"License: none shown" means the GitHub page had no LICENSE file or license label. Legally that means all rights reserved: **not reusable without permission**.

---

## 1. Tool and project table

| # | Name | URL | What it does | Formats | License | Maintained? | Reusable for MKDSrust? |
|---|---|---|---|---|---|---|---|
| 1 | **Mario Kart Toolbox (MKT)**, by HaroohiePals (Ermelber, Gericom et al.) | https://github.com/HaroohiePals/MarioKartToolbox | The modern, "fully fledged" MKDS editor. Visual editing of every NKM section, error detection, a collision (KCL) generator and viewer, 2D/3D gizmos, drawing tools, a live camera editor. Builds for Win/macOS/Linux. Character/kart editing and a ROM explorer are planned. | NKM, KCL (gen and view), NSBMD rendering | **MIT** | Yes. 257 commits, CI badge, 18 open issues. | **Yes, best candidate.** MIT is GPL-compatible. It is C#/.NET, so it would be ported or used as a reference rather than linked. The NKM error-detection rules and the KCL generator are especially valuable. |
| 2 | **MKDS Course Modifier (MKDSCM)**, by Gericom | https://fileinfo.com/software/mkds_course_modifier/mkds_course_modifier | The classic all-in-one tool. Views and edits MKDS files: kart/course collision, spawn points, cameras. Imports OBJ to NSBMD (File > New > NSBMD) and exports models to OBJ. | NKM, KCL, NSBMD, NSBTX, NSBTA, NSBCA, NARC/CARC, BMG, SPA, BTI | Freeware, closed source (no repo found; https://github.com/Gericom/MKDS_Course_Modifier returns 404) | No. Last version v4, 2015-02-26 (fileinfo). | Reference only. Superseded by MKT. |
| 3 | **EveryFileExplorer (EFE)**, by Gericom | https://github.com/Gericom/EveryFileExplorer | A plugin-based file explorer/editor. Its plugins are 3DS, NDS, Common Compressors, Common Files, Mario Kart, and LEGO Pirates. The Mario Kart plugin includes an `MarioKart.MKDS.KCL.Generate` command that turns OBJ into KCL ([source mirror](https://git.randommeaninglesscharacters.com/Gericom/EveryFileExplorer/src/branch/master/MarioKart/MarioKartPlugin.cs), seen via search snippet). | NDS FS, NARC, compressions, MKDS KCL/NKM, 3DS formats | **None shown** | Last update 2021-02-11 | No code reuse without permission. You can still read it to learn the algorithms. |
| 4 | **NitroPaint**, by Garhoogin (a CTGP Nitro programmer) | https://github.com/Garhoogin/NitroPaint | General DS 2D/texture editor | NCLR/NCGR/NSCR/NCER/NANR, NFTR fonts, BMG, **NSBMD/NSBTX** texture archives, NNS TGA, all BIOS compressions (LZ/RLE/Huffman/Diff) plus LZX/ASH | **BSD-2-Clause** | Yes. 1,388 commits, 115 stars. | **Yes.** BSD-2 is GPL-compatible. Its texture encoders (including 4x4 compressed textures) are prime reference or port material. |
| 5 | **ptexconv**, by Garhoogin | https://github.com/Garhoogin (repo list) | Command-line converter for DS textures and backgrounds | DS texture formats, NCGR/NSCR | Not checked | Unknown | Check the license. Useful as a CLI step in an asset pipeline. |
| 6 | **apicula**, by scurest | https://github.com/scurest/apicula | Rips, views and converts DS 3D models | NSBMD, NSBTX, NSBCA, NSBTP, NSBTA (experimental) to COLLADA .dae / glTF .glb | **0BSD** | Yes. 255 commits, auto Windows builds. | **Yes, ideal.** It is **written in Rust** and 0BSD (no attribution even required, but credit them anyway). Use it directly as a crate or vendored code for NSBMD parsing and glTF export. |
| 7 | **Tinke**, by pleonex | https://github.com/pleonex/tinke | Generic NDS file viewer/editor | NCLR/NCGR/NSCR/NCER/NANR, BTX0/BMD0, SDAT/SWAV/SWAR/STRM/SADL, BMG, NARC, LZ77/LZSS/Huffman/RLE | **GPL-3.0** | **Archived 2022-11-26**. The successor is SceneGate/Ekona. | Reusable (same license) but legacy C#. Prefer SceneGate/Ekona. |
| 8 | **TinkeDSi** (fork) | https://git.randommeaninglesscharacters.com/R-YaTian/TinkeDSi | Maintained Tinke fork with DSi support | as Tinke | GPL-3 inherited (not verified) | Active fork | As Tinke |
| 9 | **NitroEdit**, by XorTroll | https://github.com/xortroll/nitroedit | ROM/format editor that **runs on the DS itself** (proof of concept). Background post: https://xortroll.github.io/posts/nitroedit/ | NDS header/icon, NitroFS, NARC/CARC, NCGR/NCLR/NSCR, SDAT/SWAR/SWAV, BMG | **MIT** per the README (GameBrew wrongly says GPL-3) | PoC | Yes (MIT). Its C++ NitroFS rebuild and FAT-offset patching logic is a good reference for export-to-ROM. |
| 10 | **mkds-re**, by XorTroll | https://github.com/XorTroll/mkds-re | RE of MKDS **EU (AMCP)**: ASM-hack examples with generated headers and a symbols file (`mkds-eu-decomp-symbols.x`), an IDA export script, a web save editor (https://xortroll.github.io/mkds-re), `mkdsutil` (save encrypt/decrypt), C++ format code in `tools/common` | Save file, misc formats | **MIT** | Last push 2025-01-24 (GitHub topic page) | **Yes.** The save-format code and struct knowledge port easily. Symbols are EU, not our USA rev 0, so they would need address mapping. |
| 11 | **MKDS-decomp-headers**, by HaroohiePals (Gericom and Ermelber's private decomp) | https://github.com/HaroohiePals/MKDS-decomp-headers | Headers from the MKDS decomp "for making custom code": race, scene, ui, save, snd, particles, heap, archive, rng, etc. | C headers | **None shown** | 1 commit | Names and struct layouts are facts, but the files carry no license. **Ask Ermelber/Gericom for permission.** Very high value for naming. |
| 12 | **mkds-asm**, by rocoloco321 | https://github.com/rocoloco321/mkds-asm | "ASM template for MKDS": Makefile, `linker.x`, `symbols.x`, `arenaoffs.txt`, `material_patch.txt` | ARM9 code | None shown | **Archived.** It points to Ermelber/Custom-MKDS-Playground, which now returns 404. | Reference only |
| 13 | **RuntimeCodeModules (RCM)**, by Garhoogin | https://github.com/Garhoogin/RuntimeCodeModules | **Relocatable code patches for MKDS that load at runtime** from `code.rcm` in an archive, at race start (before map objects init) and on menus. Has a hook table (AREPL/ANSUB/TREPL/TNSUB/DATA32/DATA16/LOAD/UNLOAD) and a relocation table (ABS32/CALL/JUMP24/BASE_ABS). Restores the original bytes on unload. | `.rcm` | **Unlicense** (public domain) | Small project | **Yes, freely.** It is the best design reference for per-track code mods, and plausibly how CTGP Nitro ships per-course code (**not confirmed**). |
| 14 | **arpatch**, by Garhoogin | https://github.com/Garhoogin/arpatch | Compiles C code and a `hooks.txt` into an **Action Replay code** | AR codes | None shown | 3 commits | Idea only: "export code mod as AR cheat" is a cheap real-DS export path. |
| 15 | **mkds-tag**, by Garhoogin | https://github.com/Garhoogin/mkds-tag | Tag game mode replacing Shine Runners. Built with Szymbar's fork of **NSMB Editor 5** ("Make+Insert") on an **EUR** ROM. | ARM9 code | None shown | — | Reference |
| 16 | **GRPEdit**, by Garhoogin | https://github.com/Garhoogin (repo list) | Editor for MKDS `grpconf.tbl` (Grand Prix/cup configuration) | grpconf.tbl | Not checked | — | Format knowledge |
| 17 | **makecpic**, by Garhoogin | https://github.com/Garhoogin (repo list) | Tooling for CTGP Nitro course pictures | course pics | Not checked | — | Format knowledge |
| 18 | **LocalMapGen**, by Gericom | https://github.com/Gericom?tab=repositories | Local-map tile vector quantization for MKDS (the bottom-screen minimap) | minimap tiles | None shown | 2018 | Algorithm reference |
| 19 | **NCPatcher**, by TheGameratorT | https://github.com/TheGameratorT/NCPatcher | Universal DS code patcher. Static hooks `ncp_jump/call/hook/over/repl`, runtime patches `ncprt_*`, targets main and overlays (append/replace/create), LZ compression. Works on an extracted ROM. | ARM9/overlays | **GPL-3.0** | Active (93 commits; listed as updated Feb 2026) | **Yes (same license).** Recommended by mkds-re. Its hook syntax is a de facto community standard worth supporting. |
| 20 | **Fireflower**, by Mamma Mia Team | https://github.com/MammaMiaTeam/Fireflower | DS patching toolchain (nds-extract, nds-build, fireflower). Hooks `hook/rlnk/safe/over`. ARM9/ARM7 binaries only; overlays planned. | ARM9/ARM7 | **BSD-3-Clause** | "Not finished yet" | Yes. Predecessor of NCPatcher. |
| 21 | **NitroPacker**, by Haroohie Club (Ermelber pinned) | https://github.com/haroohie-club/NitroPacker | Unpacks/packs ROMs. `patch-arm9` and `patch-overlays` with ASM or C hacks (devkitARM or LLVM). | NDS ROM, ARM9, overlays | **GPL-3.0** | Active (v3.x, .NET 8) | Yes. Good reference for the "export mod to real ROM" step. |
| 22 | **NSMB Editor (NSMBe)** plus Szymbar's fork | https://nsmbhd.net/wiki/NSMBe_Versions/ | Historic "Make+Insert" code-injection workflow (compile, link, insert loader, hooks from a symbol file) that MKDS hackers used | ARM9 code | GPLv3-or-later per Open Hub (https://openhub.net/p/nsmb-editor), **unverified** | Legacy | Historic. Explains the community's hook naming (arepl/ansub/trepl/tnsub). |
| 23 | **Nitro Studio 2**, by Gota7 | https://github.com/Gota7/NitroStudio2, docs https://gota7.github.io/NitroStudio2/guide/fileTypes.html | SDAT sound archive editor | SDAT, SSEQ, SSAR, SBNK, SWAR, SWAV, STRM; SDK text formats; MIDI/DLS/SF2/WAV import | **None shown** | Betas via Discord | Not reusable without permission. Its feature set is the bar to match for music mods. |
| 24 | **NitroEffectMaker**, by HaroohiePals | https://github.com/HaroohiePals | Editor for the DS "Simple Particle Library" (SPA, MKDS particles) | SPA | **MIT** | — | Yes (MIT). Particle format reference. |
| 25 | **MKDS-ARM9-Music-Editor**, by LandonAndEmma (based on Ermelber) | https://www.github.com/LandonAndEmma/MKDS-ARM9-Music-Editor | Web tool to edit the music-slot table in `arm9.bin` ("any SSEQ in most slots"). A sibling **MKDS-ARM9-Slots-Swapper** swaps special slot values such as weather. | arm9.bin tables | None shown | — | Data reference only. Shows that **slot tables are hardcoded in ARM9**, a known pain point. |
| 26 | **MKDS-ARM9-Editor**, by Team-BrewSearch / NitroShellMKDS | https://github.com/Team-BrewSearch/MKDS-ARM9-Editor | .NET 10 + Avalonia editor for values in `arm9.bin` (credits ARM9 Music Editor and Weather Slot Documentation) | arm9.bin | **MIT** | Updated 2026-07-16 | Yes. Reference for which ARM9 tables people actually edit. |
| 27 | **marIOkart**, by blayyyyyk | https://github.com/blayyyyyk/marIOkart, docs https://blayyyyyk.github.io/marIOkart/api/mkds/nkm.html | AI that learns speedrun inputs. DeSmuME overlays, **Python KCL/NKM parsers** with docs, `py-desmume-mkds` memory API. | NKM, KCL, RAM | **GPL-2.0** | Active (2026-10-29) | GPL-2.0-only would conflict with GPL-3 unless it is "or later" (check). Its NKM docs are a good cross-check. |
| 28 | **MKDSTTEncoder**, by simontime | https://github.com/simontime/MKDSTTEncoder | Accurate decompilation of the MKDS time-trial code encoder | TT codes | None shown | 2 commits | Logic reference (credited by mkds-re) |
| 29 | **mkds-wiimmfi-dashboard (+ API)**, by itshichabk | https://github.com/topics/mkds | React/Express dashboard of online MKDS players on Wiimmfi | — | not checked | 2025 | Idea for a mod-hub "who's online" widget |
| 30 | **MKDSrust** (ours) | listed on https://github.com/topics/mkds | — | — | GPL-3.0 | — | — |
| 31 | **mkjs**, by riperiperi | https://github.com/riperiperi/mkjs | A JavaScript reimplementation of MKDS rendering/racing in the browser (from memory). The repo URL returned 404 in this session, so its location and license are unverified. | NSBMD/NKM/KCL (unverified) | unknown | unknown | Check whether it moved |
| 32 | **Delta Patcher / xdelta / xdelta UI / Floating IPS** | https://www.gamebrew.org/wiki/List_of_DS_PC_utilities | Patch distribution: every MKDS hack ships as .xdelta per region | xdelta/IPS/BPS | various (xdelta is Apache-2.0, from memory) | Delta Patcher updated 2025-08 | We should **emit xdelta/BPS** for DS export |
| 33 | **NDSFactory**, by Luca1991 | https://github.com/Luca1991/NDSFactory | Unpacks/repacks .nds | NDS | not checked | 1.3.3, 2025-11-28 | Reference |

Not found or not verified: "Fiddler" (no MKDS tool by that name surfaced), "KartPortal" for MKDS, "mkdsboards", "Kart Krew" (from memory, the SRB2Kart/Dr. Robotnik's Ring Racers team; no MKDS link found), Tockdom pages for MKDS (none indexed), "MKDS Every Track pack". There is also an older standalone **NKM Editor** by mkdasher, credited by Mario Kart 0 (https://www.gamebrew.org/wiki/MKDS_-_Mario_Kart_0); no repo found.

---

## 2. Major hacks / mod packs (what people actually play)

| Hack | Team | Highlights | Distribution | Source |
|---|---|---|---|---|
| **CTGP Nitro** (the flagship) | Lead SGC. Programmers Gericom, Szymbar, Ermelber, Garhoogin, Rocoloco321, SGC. Online CTs by Garhoogin, Szymbar, UltimateARHacker, SGC. | 72 new custom tracks (104 courses total), all playable online. "Custom Tracks Worldwide" matchmaking. Alt-skin system (12 skins, toggled on character select). Speedometer and PRB (Prolonged Rocket Boost) indicator. Ghost saving. Random-track options. 15% online mirror chance. Localized to EN/ES/FR/DE/IT. Preserves vanilla. Everything unlocked. v1.1.0 (2024-04-11). | xdelta per region (US/EU/JP); Korean not supported | https://www.gamebrew.org/wiki/Mario_Kart_DS_-_Custom_Tracks_Grand_Prix_Nitro_(CTGP_Nitro), https://www.speedrun.com/ctgp_nitro, site https://dswiki.garhoogin.com/ctgp-n (cert expired) |
| **Ermii Kart DS** (Legacy Ed., 2019) | Ermelber (lead, ASM), Yoshidude4, Szymbar15 (ASM), Gericom (ASM) and others | Replaces all courses, characters and karts. Alt skins (L/R). Working local/global maps. Wiimmfi online. "Sanic mode". **ROM encrypted to prevent asset extraction.** Recommends DSi clock via TWiLight/nds-bootstrap to avoid slowdown. | xdelta | https://www.gamebrew.org/wiki/Ermii_Kart_DS |
| **Mario Kart Nitro Revolution** (WIP, 2023) | rocoloco321, Antiflex, SouthPort, AltairYoshi, KrisGames4 | Brings MKWii content to MKDS. **Custom-coded map objects** (Moo Moo cows, Wiggler). Kart recolors. Any character on any vehicle. Funky Kong CPU. Custom item icons in progress. "Aims to be hardware compatible." | xdelta | https://www.gamebrew.org/wiki/Mario_Kart_Nitro_Revolution |
| Mario Kart Zero (2022) | — | Replacement tracks and custom music | — | https://www.gamebrew.org/wiki/List_of_all_DS_Rom_Hacks |
| MKDS N64 Circuit (2017), Banana Blitz (2020) | — | Retro track replacements; minihack | — | same |
| GameCube Grand Prix (2021) | — | Double Dash-themed rework. The two-driver mechanic was dropped because of "limits to what can be done". | — | https://www.nintendolife.com/news/2021/09/modders_are_reimagining_mario_kart_double_dash_on_ds |

**What is most popular:** (1) custom tracks with online play (CTGP Nitro dominates, and has its own speedrun.com board); (2) character skins/alt-skins and kart recolors; (3) custom music (music-slot editors exist); (4) speed/200cc-style mods (only AR codes found; see section 3); (5) QoL for speedrunners (speedometer, PRB indicator, ghost saving); (6) new game modes (mkds-tag); (7) custom map objects (Nitro Revolution).

---

## 3. Code hacking for MKDS

### 3.1 Injection approaches (in historical order)
1. **Action Replay codes.** The oldest and most common route. Almar's USA AR list: https://almarsguides.com/retro/walkthroughs/NDS/Games/MarioKartDS/ActionReplay/. SuperCheats has per-code pages (e.g. https://www.supercheats.com/nintendods/mario-kart-ds/3673/ar-codes/character-modifier-codes/). Kodewerx request thread: https://www.kodewerx.org/forum/viewtopic.php?p=34015. Examples there:
   - **"300cc" speed code (E/AU):** `6217ad18 00000000 / b217ad18 00000000 / 000000d4 0000XXXX / 000000d8 0000XXXX / 000000dc 0000XXXX / d2000000 00000000`. The default is 1000; 2000 is about 300cc. Values above that make karts fly off on small bumps. It reads a pointer at 0x0217AD18 (EU) and writes three speed fields at +0xD4/+0xD8/+0xDC. **This is the community's "200cc".** No proper 200cc ASM mod (with brake/drift tuning like MKWii/MK7's) turned up in search.
   - Unlock-all (EU): `023CE2E0 07FFFFFF`. XorTroll traced this to a fixed heap address of the savedata object (the DS has no ASLR and heap allocations happen in a deterministic order): https://xortroll.github.io/posts/mkds-mem-cheats
   - Requests from that thread that show what people want: scroll through items, mushroom boost on mini-turbo, Lakitu-pickup button, cruise control/auto-snake, offline tracks online, Wi-Fi emblem colors.
   - **arpatch** (Garhoogin) compiles C into AR codes: https://github.com/Garhoogin/arpatch
2. **NSMBe "Make+Insert"** (Szymbar fork): compiles C/ASM, inserts a loader and hooks via `arenaoffs.txt` (one RAM address) and a symbol file. Used by mkds-tag (https://github.com/Garhoogin/mkds-tag) and the mkds-asm template (https://github.com/rocoloco321/mkds-asm). Workflow description: https://nsmbhd.net/post/23905/
3. **Fireflower → NCPatcher**: the modern static patchers (hooks, overlay targets, LZ). NCPatcher is recommended by mkds-re. https://github.com/TheGameratorT/NCPatcher
4. **NitroPacker** `patch-arm9`/`patch-overlays`: https://github.com/haroohie-club/NitroPacker
5. **Runtime Code Modules**: per-archive relocatable `code.rcm` loaded at race start and on menus, with hook table, relocations and unload/restore. https://github.com/Garhoogin/RuntimeCodeModules. This is the most "mod-system-like" design in the scene and fits per-course code (custom objects, track gimmicks).

### 3.2 Symbols / hook addresses
- **Region gap:** almost all public symbol work targets **EU (AMCP)**: mkds-re's `mkds-eu-decomp-symbols.x`, the mem-cheats post, and mkds-tag (EUR ROM). mkds-asm's `symbols.x` region is unknown. **Nothing public targets USA rev 0 (AMCE) with full symbols.** MKDSrust's USA work would fill that gap. An EU↔US address map would let us import their symbols.
- EU memory layout (XorTroll): ARM9 about 0x02000000–0x02180000, overlays about 0x02180000–0x021C0000, ARM7 about 0x02380000–0x023A8000, heap 0x021DA340–0x023E0000.
- Gericom and Ermelber hold a private, more complete decomp. Its headers are partly published (MKDS-decomp-headers). Gericom also wrote **Derailer**, a decompiler for the Metrowerks compiler: https://github.com/Gericom?tab=repositories

### 3.3 Item / AI / gameplay mods
No public, licensed source for item-distribution or AI mods turned up. Nitro Revolution's custom item icons are "preliminary". CTGP Nitro's code is not public; I found no repo.

---

## 4. Pain points and wishes (as far as public evidence shows)

The scene lives mostly in Discord (the CTGP Nitro Discord is linked from GameBrew), so public complaint threads are scarce. GBAtemp and speedrun forums returned 403. These points come from tool READMEs, hack pages and patterns:

1. **Tools are Windows-only and old.** MKDSCM's last release was 2015 and it is closed source. EFE has no license and its last update was 2021. Guides say you need *both* MKDSCM 3.0 and an older version for model export (https://www.tapatalk.com/groups/zelda64/nds-how-to-rip-nds-game-models-t110.html). MKT is the fix but is still filling gaps (characters/karts and the ROM explorer are still "planned").
2. **No end-to-end custom-track pipeline.** You need a separate model converter (OBJ to NSBMD), a separate KCL generator, an NKM editor, texture tools (NitroPaint), music tools (Nitro Studio 2), ARM9 slot-table editors, then a ROM packer and an xdelta maker. The search found no single current tutorial.
3. **Hardcoded tables in arm9.bin.** Music slots, weather slots and cup configuration (`grpconf.tbl`) each need their own little editor (Music Editor, Slots Swapper, GRPEdit). Adding tracks beyond the vanilla slots needs ASM. CTGP Nitro's 104-course menu with cup scrolling is a large custom-code effort.
4. **Hardware limits.** DS RAM and polygon budgets, and slowdown that pushes people to the DSi clock (Ermii Kart recommends TWiLight/nds-bootstrap; CTGP-N users are told to disable SWI Halt Hook; https://www.speedrun.com/ctgp_nitro/guides/r4ux0 via search). Mechanics like Double Dash's two drivers are judged infeasible.
5. **Region fragmentation.** Hacks ship separate US/EU/JP patches, Korean is unsupported, and the RE work is EU-centric.
6. **Code hacking has a steep learning curve.** You need devkitARM, symbol files and hook syntax. GNU make breaks on paths with spaces (RCM, NSMBe). No high-level scripting exists.
7. **Asset protection vs. openness.** Ermii Kart encrypted its ROM to stop asset extraction, which shows creators care about crediting and protecting their custom assets.
8. **Online compatibility.** All players must run the same hack release (CTGP-N online is Friends-Only plus its own worldwide CT mode). Getting online on DS hardware is clunky (WEP/open Wi-Fi only).
9. **Speedrunner QoL** (snaking with the D-pad is tiring; reddit mirror via search: https://reddit.rtrace.io/r/MarioKartDS). They want speedometers, PRB indicators and ghosts, which CTGP-N added.

---

## 5. Online revival (context only)

- **Wiimmfi** is the main live service for MKDS. Patch ROMs with WfcPatcher (`--domain wiimmfi.de`) or set the DNS to a Wiimmfi address (https://wiki.ds-homebrew.com/ds-index/wifi). Error troubleshooter: wiimmfi.de/error.
- **Kaeru WFC** (https://kaeru.world/projects/wfc/) is a DNS-only route (no patches). It forges certificates the DS accepts via **nds-constraint** (https://github.com/KaeruTeam/nds-constraint; no license shown), a DS SSL flaw in intermediate-certificate checks, and proxies to Wiimmfi.
- **AltWFC / dwc_network_server_emulator** (https://github.com/barronwaffles/dwc_network_server_emulator) is a Python 2.7 DS/Wii server emulator, **AGPL-3.0**, 1.2k stars.
- **WiiLink WFC** has an MKDS page (https://wfc.wiilink24.com/online/mariokartds).
- The **Delta** iOS emulator added DS online via Wiimmfi/AltWFC/Kaeru (https://www.timeextension.com/news/2025/04/iphones-delta-emulator-has-just-got-online-play-for-nintendo-ds-games).
- CTGP Nitro works with "any Nintendo WFC replacement" and adds Custom Tracks Worldwide.
- Implication for MKDSrust: for PC↔DS crossplay we would have to speak DWC/GameSpy plus MKDS's peer protocol (mkds-re credits the Custom Mario Kart Wiki for WFC fields; MKDS-decomp-headers has `rnet.h` and `peerMatch.h`). AltWFC is AGPL, which is GPL-3-compatible for reference and linking.

---

## 6. Licensing summary for reuse

| Usable in GPL-3 MKDSrust (with credit) | Same-license / compatible | Do not copy without permission |
|---|---|---|
| apicula (0BSD, **Rust**), RuntimeCodeModules (Unlicense), Mario Kart Toolbox (MIT), NitroEffectMaker (MIT), mkds-re (MIT), NitroEdit (MIT), MKDS-ARM9-Editor (MIT), NitroPaint (BSD-2), Fireflower (BSD-3) | NCPatcher (GPL-3), NitroPacker (GPL-3), Tinke (GPL-3), AltWFC (AGPL-3, network clause applies if we host it), marIOkart (GPL-2.0, check whether "or later") | EveryFileExplorer, MKDS-decomp-headers, Nitro Studio 2, mkds-asm, arpatch, mkds-tag, GRPEdit?, MKDS-ARM9-Music-Editor, MKDSTTEncoder, nds-constraint, MKDS Course Modifier (closed), CTGP Nitro (closed) |

Facts (offsets, struct layouts, format specs) are not copyrightable in themselves, but copying header files verbatim is copying. Rewrite them from our own decomp, cite the source, and ask the authors when in doubt.

---

## 7. What MKDSrust should adopt / learn (concrete recommendations)

1. **Use apicula as our NSBMD/NSBTX/NSBCA/NSBTP layer.** It is Rust and 0BSD. Vendor it or depend on it for model loading and glTF export, and credit scurest. Pair it with a glTF→NSBMD writer (the missing piece MKDSCM's closed importer used to fill) for DS export.
2. **Port Mario Kart Toolbox's NKM model, NKM validation rules and KCL generator** (MIT) to Rust, crediting HaroohiePals. Or interoperate: read and write the same NKM so MKT stays the track editor, and MKDSrust becomes the native "play it now" target plus the DS-compatibility checker. Contact Ermelber/Gericom early. They are the core of CTGP-N, Ermii Kart, MKT and the private decomp, and their blessing matters most in this scene.
3. **Adopt the community's hook vocabulary for code mods.** Accept NCPatcher-style (`ncp_hook/ncp_call/ncp_jump/ncp_over/ncp_repl`) and RCM-style (`AREPL/ANSUB/TREPL/TNSUB/DATA32/LOAD/UNLOAD`) hook declarations. On PC they map to Rust/Lua function overrides. For DS export, emit an NCPatcher project or an RCM `code.rcm`. This "one mod source, two targets" plan is the killer feature nobody has.
4. **Model the mod system on RCM's lifecycle.** Per-track (per-archive) code that loads at race start and on menus, unloads cleanly and restores the original bytes. Use the same scopes in MKDSrust: global mods, per-course mods, per-menu mods.
5. **Turn hardcoded ARM9 tables into data.** Music slots, weather slots, grpconf/cups, character/kart stats, and course lists should all become TOML/JSON in MKDSrust, with an unlimited track list and cup scrolling on PC. For DS export, write them back to the arm9.bin offsets that the Music Editor / ARM9-Editor community already uses (verify for USA).
6. **Build the "DS compatibility checker"** around the known hardware limits: polygon/vertex counts per NSBMD, texture VRAM (512 KB textures, palette limits), KCL size and octree depth, NKM entity caps, RAM/heap budget (EU heap 0x021DA340–0x023E0000 as reference), SSEQ channel usage, ROM size. Report each as "OK on DS / DSi-clock only / PC only". This directly answers the slowdown pain point.
7. **Export as xdelta/BPS per region, never a full ROM.** That matches community norms (every hack ships xdelta) and our no-Nintendo-content rule. Optionally emit an **AR code** for small code mods (the arpatch idea) and an **NCPatcher/NitroPacker project** for big ones.
8. **Provide a USA (AMCE) symbol map plus an EU↔USA address translator.** All public RE (mkds-re, mem-cheats, mkds-tag) is EU-only. Publishing USA symbols under GPL fills a real gap and lets us import EU knowledge such as cheat addresses.
9. **Ship speedrunner QoL as first-party example mods:** speedometer, PRB indicator, ghost save/load, TT-code encoder (port MKDSTTEncoder logic after asking simontime), input display, a cruise-control/snake-assist toggle (requested since 2008). Also a proper **200cc** (speed plus brake/drift tuning, replacing the crude speed-pointer AR code) and an **item-distribution editor**. These are low-risk showcases.
10. **Save-file compatibility:** port mkds-re's save encrypt/decrypt (MIT) so PC saves import from and export to real DS saves and the web save editor.
11. **Make "Alt skins / any character on any kart" a built-in system,** since CTGP-N, Ermii and Nitro Revolution all reinvented it.
12. **Respect asset ownership:** mod manifests should have author/credit/license fields and an optional "no-extract" flag (Ermii Kart cared enough to encrypt). Mod packages ship only creator assets and never vanilla files, with diffs against the user's ROM.
13. **Online, later:** keep the PC netcode compatible in principle with Wiimmfi/Kaeru (DWC plus MKDS peer protocol), so PC players could someday join DS rooms. Study AltWFC (AGPL) for the server side. Defer, per the scope freeze.
14. **Cross-platform and no-toolchain setup** (a direct answer to the devkitARM/make/space-in-path pain): MKDSrust's built-in compiler path for mods should not need devkitARM unless exporting to DS, and should handle paths with spaces.
15. **Interop rather than replace:** import NitroPaint/Nitro Studio 2 outputs (NCGR/NCLR, SDAT/SSEQ, SF2/MIDI) instead of rebuilding every editor. The community already has good 2D and sound tools. MKDSrust's unique value is native play, code mods with a DS export, and compatibility checking.
