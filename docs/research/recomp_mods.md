# Recompilation / decomp-port projects and their mod ecosystems — lessons for MKDSrust

Research date: 2026-11-11. Sources: project READMEs/docs fetched live. Where a page did not state something, this report says "not stated" rather than guessing. Items marked *(inference)* are my reading, not a quoted claim.

---

## 0. Headline findings

1. **There is already a shipping MKDS static recomp: DS Kart Recompiled (AlonsoTlol / "alontube").** v1.0, Windows, EU ROM (AMCP) only, MIT, co-written with Claude. It builds the port locally from the user's ROM in ~5 min on 8 cores, ~200 MB first-run download, 1.5 GB disk. It has enhancements (2x–4x 3D, 240 fps interpolation, 16:9 layouts, online/LAN rooms, replay capture) but **no mod system** is described. MKDSrust's differentiators therefore have to be: a mod API + mod manager, readable named source, US+EU support, and light modular downloads. https://github.com/AlonsoTlol/DS-Kart-Recompiled
2. **The N64Recomp ecosystem is the best modding model to copy.** It has declarative hooks/patches/events/exports, a `mod.toml` manifest with deps and typed config options that the launcher renders automatically, a single-file zip package (`.nrm`), conflict detection, and Thunderstore hosting (187 packages for Zelda 64: Recompiled). https://hackmd.io/@Wiseguy/HyhUXwiV0, https://hackmd.io/fMDiGEJ9TBSjomuZZOgzNg, https://thunderstore.io/c/zelda-64-recompiled/
3. **sm64coopdx is the best scripting model.** It uses Lua with `hook_event(HOOK_*)`, action/behavior overrides, auto-generated mod-menu widgets, a folder-with-`main.lua` layout and drag-and-drop install, plus its own mod site. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/guides/hooks.md
4. **Ship of Harkinian's GameInteractor is the best model for a curated, high-level event API**, especially its "vanilla behavior" (`should`) hooks, which let mods veto or alter specific game decisions without replacing functions. https://raw.githubusercontent.com/HarbourMasters/Shipwright/develop/soh/soh/Enhancements/game-interactor/GameInteractor.h
5. **The legal patterns split two ways:**
   - **(a) Ship the recompiled binary, user supplies the ROM for assets:** N64Recomp ports and Unleashed.
   - **(b) Ship only your own code and generate/compile everything ROM-derived locally:** DS Kart Recompiled, Phantom Hourglass recomp, gbarecomp/MKSC.

   MKDSrust's stated policy is (b), the more conservative pattern. Recent DS/GBA projects show (b) can still be one-click UX.

---

## 1. N64Recomp + Zelda64Recomp (Majora's Mask) + RT64

### Approach
- **N64Recomp (MIT)** splits a MIPS binary into functions using symbol metadata (currently from an ELF) and emits one C function per function, instruction by instruction. `jal` becomes a C call, jump tables become switches, and overlays (static and relocatable) are handled through runtime lookups and RELOC_HI16/LO16 macros. Config is TOML with stubs, skips and instruction patches. TOML-level hooks are listed as "currently unimplemented". https://github.com/N64Recomp/N64Recomp
- **N64ModernRuntime (GPL-3.0)** = `ultramodern` (libultra reimplementation: threads, audio, VI timing, RSP tasks, with host callbacks for I/O) + `librecomp` (bridge for overlays, PI DMA, saves). https://github.com/N64Recomp/N64ModernRuntime
- **RT64 (MIT)** is the renderer: D3D12, Vulkan and Metal, DDS texture packs with async streaming, HFR interpolation, widescreen and ultrawide. https://github.com/rt64/rt64
- Menus and launcher use **RmlUi** (HTML/CSS-like UI). https://www.github.com/Mr-Wiseguy/Zelda64Recomp, https://github.com/mikke89/RmlUi

### Legal / shipping
- The repo and releases contain no game assets. The user selects their US ROM in the main menu, and assets load from it. It "is not an emulator and it cannot run any arbitrary ROM". **Prebuilt binaries are on Releases.** https://github.com/Zelda64Recomp/Zelda64Recomp
- Building needs the decompressed US ROM (SHA-1 checked). You run `N64Recomp us.rev1.toml` plus RSPRecomp, then CMake/clang. https://github.com/Zelda64Recomp/Zelda64Recomp/blob/dev/BUILDING.md
- So the shipped exe contains code generated from the ROM, and only assets come from the user's copy *(inference from the above)*. Press coverage frames the distributed code as original work and the assets as user-supplied. https://heldgames.com/guides/n64-recompiled-games-list
- License: Zelda64Recomp GPL-3.0. https://github.com/Zelda64Recomp/Zelda64Recomp

### Launcher / UX
- In-game RmlUi menu: graphics, audio, input, gameplay. Portable mode is enabled by a `portable.txt` beside the exe, which keeps saves, configs and mods together. Flatpak and Steam Deck instructions are provided. https://github.com/Zelda64Recomp/Zelda64Recomp

### Mod system (key reference)
- **Package `.nrm`** is "just a renamed zip". Contents:
  - a JSON manifest (the only mandatory file)
  - an optional mod symbol file and a self-contained MIPS binary, **recompiled at load time**
  - optional textures (`.rtz` texture packs with `rt64.json`)
  - an optional base-ROM patch file
  - an optional `thumb.png`/`thumb.dds`

  https://hackmd.io/fMDiGEJ9TBSjomuZZOgzNg
- **Build**: mod authors write C against the decomp headers, compile with clang (MIPS target, LLVM 18.1.8), and run `RecompModTool mod.toml build/`, which outputs the `.nrm`. https://github.com/Zelda64Recomp/MMRecompModTemplate
- **Manifest (`mod.toml`)** sections: `[manifest]`, inputs, dependencies, config options.
  - `[manifest]` fields: id, version, display name, description, short description, authors, game id `mm`, minimum recomp version
  - Dependencies: required, optional, and native libraries
  - Inputs: ELF, output name, reference symbol files
  - Config options: enum, number (min/max/step) and string types

  https://raw.githubusercontent.com/Zelda64Recomp/MMRecompModTemplate/main/mod.toml
- **Code API: declarative, implemented as linker section attributes.** The tool reads the section names.
  - `RECOMP_PATCH`: replace a named game function
  - `RECOMP_FORCE_PATCH`: replace even a function the base recomp already patched (discouraged)
  - `RECOMP_HOOK(func)` / `RECOMP_HOOK_RETURN(func)`: run code at a function's entry or return. Several mods can hook the same function.
  - `RECOMP_DECLARE_EVENT(fn)`: a mod defines an event
  - `RECOMP_CALLBACK("mod", event)`: subscribe to an event. `*` means base-game events and `.` means your own.
  - `RECOMP_EXPORT` and `RECOMP_IMPORT("mod", sig)`: inter-mod function calls

  https://raw.githubusercontent.com/Zelda64Recomp/MMRecompModTemplate/main/include/modding.h, https://hackmd.io/@Wiseguy/HyhUXwiV0
- **Conflict rules:**
  - Two mods replacing the same function are marked incompatible.
  - Two mods with ROM patch files are marked incompatible.
  - Replacing a function the base recomp already patched makes the mod fail to load unless it is force-patched.
  - Hooks are preferred because they compose.

  Hook ordering is not documented. https://hackmd.io/@Wiseguy/HyhUXwiV0
- **Native libraries**: a mod may ship .dll/.so/.dylib files and list their exported functions in the manifest. The author must build each platform. https://hackmd.io/@Wiseguy/HyhUXwiV0
- **Common APIs**: `recompconfig.h` (config read), `recompui.h` (mods can build UI), memory alloc and data structures. Game-specific APIs are in `z64recomp_api.h`. https://hackmd.io/@Wiseguy/HyhUXwiV0
- Texture-pack toggle convention: a config option id `_recomp_texture_pack_enabled` (Enum with 2 values). https://hackmd.io/fMDiGEJ9TBSjomuZZOgzNg
- **Install UX**: drag mod files onto the game window, or use "Install Mods" in the mod menu, then toggle mods there. https://github.com/Zelda64Recomp/Zelda64Recomp
- **Distribution**: Thunderstore community `zelda-64-recompiled`.
  - 187 packages. Categories: Audio, Libraries, Misc, Modpacks, Mods, Tools.
  - Top packages (as listed on the community page): Music Randomizer ~3.9K, Darker Nights ~3.8K, Hero Mode ~3.2K. https://thunderstore.io/c/zelda-64-recompiled/
  - The README tells users to use **Manual Download**; r2modman / Thunderstore Mod Manager is "not required or supported". https://github.com/Zelda64Recomp/Zelda64Recomp
  - Ecosystem: there are API/library mods that other mods depend on (e.g. Player Model Manager), an Archipelago randomizer mod, and even a Python-bridge mod (RecompExternalPython). https://old.thunderstore.io/c/zelda-64-recompiled/p/YAZMT/Player_Model_Manager/v/0.6.1/, https://thunderstore.io/c/zelda-64-recompiled/p/RecompRando/MMRecompRando/, https://lt-schmiddy.github.io/docs/REPY_for_N64Recomp/index.html
- Mod support arrived in v1.2.0 (May 2025). https://gamingonlinux.com/2025/05/zelda-64-recompiled-majoras-mask-adds-modding-support-texture-pack-support-optimizations-and-more
- The same mod system is reused by other recomps, e.g. Mario Kart 64: Recompiled (GPL-3.0), which has an MK64 mod template, drag-drop install and toggles. https://github.com/sonicdcer/MarioKart64Recomp

---

## 2. XenonRecomp / Unleashed Recompiled (Sonic)

### Approach
- **XenonRecomp (MIT)** translates Xbox 360 PPC to C++. Its output is "not very human-readable". The TOML config drives function boundaries and jump tables.
- Hooks:
  - **function overrides** via weak linking, which lets you replace a function and still call the original
  - **`[[midasm_hook]]`**: TOML entries with name, address, registers and after_instruction, plus return/jump control-flow options

  It provides no runtime. https://github.com/hedge-dev/XenonRecomp

### Legal / shipping
- Unleashed Recompiled is GPL-3.0 and "does not include any game assets". The user dumps the game, title update and optional DLC from their own console. https://github.com/hedge-dev/UnleashedRecomp
- Building needs `default.xex`, `default.xexp` and `shader.ar` in `UnleashedRecompLib/private/`, and the first compile is slow because of code and shader recompilation. https://github.com/hedge-dev/UnleashedRecomp/blob/main/docs/BUILDING.md
- Prebuilt releases exist *(as with N64Recomp, the release contains recompiled code; the user supplies game data)*.

### Installer UX (worth copying)
- Two modes:
  - **Add Files** accepts container or image dumps and auto-detects their contents.
  - **Add Folder** accepts already-extracted files.
- **Integrity checks** reject compressed archives, modified files and mixed regions. DLC can be added later from the title screen.
- Install size is ~6 GiB (no DLC) to ~10 GiB (with DLC).

https://github.com/hedge-dev/UnleashedRecomp, https://github.com/hedge-dev/UnleashedRecomp/blob/main/docs/DUMPING-en.md

### Mods
- The built-in mod loader is driven by the external **Hedge Mod Manager** (HMM).
- Mods use the Sonic Generations `mod.ini` + files-folder format; the old UMM format is deprecated.
- Mods that replace `default.xex` or shaders are unsupported. **Code mods are not yet possible**; a few built-in "codes" can be toggled in HMM's Codes tab.

https://github.com/hedge-dev/UnleashedRecomp

HMM installs zips by drag-drop or Add Mod, launches the game with the selection applied, and supports GameBanana 1-click links. https://mintlify.wiki/hedge-dev/UnleashedRecomp/mods/installing-mods, https://flathub.org/fa/apps/io.github.hedge_dev.hedgemodmanager

**Lesson:** it reuses an existing community mod manager and format rather than inventing one. For MKDS, the analogous existing assets are the custom-track formats (NKM/NARC/CTGP-N layouts).

---

## 3. Ship of Harkinian (OoT) / 2Ship2Harkinian (MM): HarbourMasters

### Approach
- These are true decompilation ports: the matching decomp source is built natively on **libultraship (LUS, MIT)**, which reimplements libultra.
- LUS provides a resource manager over archives: `.otr` (MPQ) and `.o2r` (ZIP).
- UI is ImGui. LUS also bundles **libtcc (LGPL-2.1) "C compiler used for the script system"**. https://github.com/Kenix3/libultraship

### Legal / shipping
- "Any copyrighted assets are extracted from the ROM and reformatted as a .o2r archive". On first launch the game extracts assets into `oot.o2r` and verifies supported dumps by SHA-1 (`docs/supportedHashes.json`, ship.equipment). The program binary contains no ROM-derived code because it is built from the decomp. https://github.com/HarbourMasters/Shipwright, https://the-harbour-ashen.vercel.app/docs/ship-of-harkinian/mod-development/
- The OTR stores the SoH version, which is checked on launch and offers regeneration (version-locked extracted data). https://giteapons.duckdns.org/nickpons666/Shiip-of-Hakinian-Espanol/src/commit/7e9efeeadb80a48279cc5744e323b1844a8cda95
- Licenses:
  - 2Ship2Harkinian: CC0-1.0. https://github.com/HarbourMasters/2ship2harkinian
  - Shipwright: not stated on the README. https://github.com/HarbourMasters/Shipwright

### Mods / enhancements
- **Asset mods**: drop `.otr`/`.o2r` files into `mods/`. They are authored with "retro" (an OTR generator) or the fast64 Blender plugin. https://github.com/HarbourMasters/Shipwright
- The mod menu now handles **load order only**, with drag-and-drop rows, order retention, and pruning of removed mods; "Enable Alternate Assets" became "Enable Mods", on by default. https://giteapons.duckdns.org/nickpons666/Shiip-of-Hakinian-Espanol/src/commit/dc5e9686c0a5c1b4f1cdf285ad8bf581be6233e1
- **Code "mods" are in-tree enhancements** toggled by CVars in the ImGui menu bar, plus presets and a randomizer. https://code.moparisthebest.com/mirror/Shipwright/src/branch/develop/soh/soh/SohMenuBar.cpp
- **GameInteractor hook bus:**
  - `RegisterGameHook<H>`, `…ForID`, `…ForPtr`, `…ForFilter`, with deferred `UnregisterGameHook`
  - `COND_HOOK` and `COND_VB_SHOULD` macros to (un)register a hook when a setting changes

  "Hooks should be idempotent and execution order is not guaranteed". https://raw.githubusercontent.com/HarbourMasters/Shipwright/develop/soh/soh/Enhancements/game-interactor/GameInteractor.h
- **Hook table** examples:
  - `OnGameFrameUpdate()`, `OnActorInit/Update(void*)`
  - `ShouldActorUpdate(actor,bool*)`, `OnSceneInit(int16)`, `OnItemReceive(GetItemEntry)`
  - `OnPlayerHealthChange`, `OnOpenText(textId*, bool*)`
  - `OnVanillaBehavior(GIVanillaBehavior flag, bool* result, va_list)`: a single enum of "decision points" where vanilla logic asks whether it should do X

  https://raw.githubusercontent.com/HarbourMasters/Shipwright/develop/soh/soh/Enhancements/game-interactor/GameInteractor_HookTable.h

**Lesson:** the high-level semantic hooks plus `should`-style decision points are far more stable across versions than raw function hooks, and are easier for LLMs and modders to use.

---

## 4. sm64ex / sm64coopdx

### Approach and legal
- A decomp-based PC port. Users obtain the ROM and compile themselves; the developers do not distribute the ROM or compiled builds containing assets. Wikipedia calls this a legal "gray area" [citation needed] and describes the project as "source-available". https://en.wikipedia.org/wiki/Super_Mario_64_Coop_Deluxe
- The repo has `extract_assets.py` and `sm64.*.sha1` files. https://github.com/coop-deluxe/sm64coopdx
- It continues sm64ex-coop; v1.0 shipped July 2024. Popularity: ~1.5k stars, 389 forks, 5,127 commits. https://github.com/coop-deluxe/sm64coopdx

### Mod system (key reference for Luau)
- **Layout**: `mods/` in the base directory or the save directory. A single `.lua` file is a mod; a multi-file mod is a folder with `main.lua`. Mods can be dragged onto the window. DynOS actors go in `<mod>/actors/`. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/lua.md
- **Hooks**:
  - `hook_event(HOOK_UPDATE | HOOK_MARIO_UPDATE | HOOK_ON_LEVEL_INIT | HOOK_ALLOW_INTERACT …, fn)`
  - `hook_mario_action` (new or overridden player states), `hook_behavior` (object behaviors)
  - `hook_chat_command`, `hook_on_sync_table_change`
  - `hook_mod_menu_text/button/checkbox/slider/inputbox` (auto UI)

  Caveat: the `HOOK_ALLOW_*` return conventions are inconsistent (some need `true` to allow, others `false` to prevent), which is an anti-pattern to avoid. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/guides/hooks.md
- **Networking state** goes through sync tables (`gGlobalSyncTable`, `gPlayerSyncTable[]`, with index translation). Storage goes through a "ModFS" guide. Docs cover Globals, Constants, Functions and Structs, plus a VS Code setup guide. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/lua.md
- **DynOS** model packs (`dynos/packs`, toggled in Options → DynOS Packs), e.g. Render96. https://github.com/render96/render96ex
- **Distribution**: its own forum-style site, mods.sm64coopdx.com, with ratings and update threads, plus itch.io and GitHub collections. https://mods.sm64coopdx.com/mods/character-movesets.191/updates, https://sm64coopdx.com, https://github.com/AgentXLP/coopdx-mods

**Lesson:** the API docs are generated from the bindings, and a "VS Code setup" guide gives typed autocompletion. Both are cheap and large wins for LLM-assisted modding.

---

## 5. DS and other ARM static recompilers (most directly relevant)

| Project | Target | Approach | Legal / ROM flow | Mods | License |
|---|---|---|---|---|---|
| **DS Kart Recompiled** (AlonsoTlol) | MKDS EU (AMCP) | ARM9+ARM7 to C; custom runtime (memory, IRQ, timers, 2D/3D, sound, saves), CPU 3D by default, experimental GPU 3D | Launcher downloads Python + compiler on first run, translates the user's ROM and compiles locally. Ships only its own code. ~5 min on 8 cores, ~200 MB download, 1.5 GB disk. Localised launcher with **Update** button. | Enhancements only (res, 240 fps, layouts, online beta, replay); no mod system described | MIT |
| **Phantom Hourglass recomp** (dguillot-gh) | Zelda PH | ARM9 + 62 overlays (24,621 functions) to C; melonDS supplies ARM7/GPU/sound via `ntr-runtime` (from FFCCEOT-Recomp) | `RUN-ALL.ps1` installs tools, recompiles locally, SHA-1 dump check; "don't share builds"; ~10 GB disk; Android APK path | Fixes and options; no mod API | GPL-3.0 |
| **ndskit** (vs-sr-dev) | Game-agnostic DS toolkit (Moon, Nostalgia, Lufia, FFXII RW) | Python analysis/recompiler to C++ per program/overlay, interpreter fallback; C++20 runtime (SDL3, GL 4.5); overlay residency by CRC32 on dirty pages | BYO cartridge image; ports are separate repos with ndskit submodule + `.recomp.json` status | None yet ("recompiler has no hooks yet") | MIT |
| **gbarecomp / MarioKartSuperCircuitRecomp** (mstan) | GBA | ARM/Thumb to sharded C++, interpreter fallback with persistent native cache; shared **recomp-ui** launcher (Dear ImGui) | Never distributes ROM-derived generated code; local generation; per-revision SHA-1; "sanitized" release packaging script | **`.gbamod` = data + manifest, no arbitrary DLLs**; native behaviour compiled in and registered under trusted plugin IDs; checks ROM, feature deps and conflicts before activation; never modifies cartridge | PolyForm Noncommercial (not GPL-compatible) |
| **ds-decomp / dsd** (AetiasHax) | DS decomp toolkit (Rust) | rom extract/build, init, delink to ELF, lcf, objdiff integration, symbol apply, `diff new/apply` to port work between versions | n/a (decomp tooling) | n/a | MIT |
| **mkdsdecomp / mkds-re** (XorTroll) | MKDS EU decomp / RE | WIP C decomp, headers, ASM-hacking examples, save editor | n/a | n/a | MIT |
| Others | DQ9-Remastered (research), ndsrecomp (pre-alpha), Jump Super Stars recomp | | | | |

Sources: https://github.com/AlonsoTlol/DS-Kart-Recompiled (license: https://github.com/AlonsoTlol/DS-Kart-Recompiled/blob/main/LICENSE), https://github.com/dguillot-gh/phantom-hourglass-recomp, https://github.com/vs-sr-dev/ndskit, https://github.com/mstan/gbarecomp, https://github.com/mstan/MarioKartSuperCircuitRecomp, https://github.com/AetiasHax/ds-decomp, https://awesome.ecosyste.ms/projects/github.com%2Fxortroll%2Fmkdsdecomp, https://github.com/dq9-remastered/DQ9-Remastered, https://github.com/RetroPortingToolKit/ndsrecomp, https://github.com/deepdeep86/Jump-Super-Stars-PC-Android-Browser-port-

**The tylzars MKDS matching decomp could not be found** by web search. Only XorTroll's projects surfaced; check directly on GitHub or decomp.dev. https://awesome.ecosyste.ms/projects/github.com%2Fxortroll%2Fmkdsdecomp

**Mario Kart Wiicompiled** (Patchzy, announced July 2026) claims to be the first Wii static recomp. Its headline mod feature is compatibility with the existing **Retro Rewind** custom-track pack. https://www.tomshardware.com/video-games/retro-gaming/mario-kart-wii-recompiled-for-pc-using-ai-with-4k-potential-and-uncapped-frame-rates-first-static-recompilation-of-a-wii-game-supports-over-200-tracks-thanks-to-retro-rewind-compatibility

- **Lesson for MKDSrust:** support existing MKDS community content, especially **CTGP Nitro** and custom tracks. CTGP-N is distributed as patched ROMs/`.nds` via itch.io/GameBrew, with legality debated. https://www.gamebrew.org/wiki/Mario_Kart_DS_-_Custom_Tracks_Grand_Prix_Nitro_(CTGP_Nitro), https://xblade720.itch.io/ctgp-nitro

**Note:** AI-assisted development drew backlash for Wiicompiled. DS Kart Recompiled openly credits Claude. Expect similar reactions and be transparent. https://www.gamesradar.com/games/racing/mario-kart-wii-is-being-rebuilt-for-pc-but-the-projects-ai-vibe-coding-instantly-soured-many-nintendo-fans-on-the-project/

---

## 6. Mod distribution platforms

### Thunderstore
- **Package**: a zip with `manifest.json`, `README.md` and a 256×256 `icon.png` at the root (optional CHANGELOG.md).
- **Manifest**: `name` ([A-Za-z0-9_], ≤128), `description` (≤250), `version_number` (semver, compared numerically), `dependencies` (`Team-Package-1.2.3` strings, auto-installed by managers), `website_url`.
- Soft size limit 10 GB. https://wiki.thunderstore.io/mods/creating-a-package
- **Adding a game**: request it in the Thunderstore Discord #game-requests. The team adds an ecosystem-schema PR, then media, then optional mod-manager support. It prefers existing dev interest and mods ready to upload. https://wiki.thunderstore.io/ecosystem/adding-a-new-game
- **r2modman** (MIT) offers profiles and profile export codes to share mods + configs. New games now appear automatically. https://github.com/ebkr/r2modmanPlus, https://old.thunderstore.io/package/ebkr/r2modman/3.1.35/
- The N64 recomps use Thunderstore purely as hosting and manual download. https://github.com/Zelda64Recomp/Zelda64Recomp
- Note: dependency strings pin **exact** versions. Managers typically install the pinned or newer version *(range semantics are not documented on the wiki page)*.

### GameBanana
- A long-standing hub for Sonic/Nintendo-scene mods. HMM integrates via GameBanana **1-click install**, which works through a custom URL scheme registered by the manager.
- The API (`https://gamebanana.com/apiv11`) is used by third-party managers. Official docs are thin; a community Postman collection exists. https://flathub.org/fa/apps/io.github.hedge_dev.hedgemodmanager, https://www.mintlify.com/Pucas01/ZZAR/features/gamebanana-browser, https://github.com/s0nought/gb-api-v11
- The URL-scheme pattern (e.g. `vintagestorymodinstall://`, `x-scheme-handler` on Linux). https://mods.vintagestory.at/show/mod/8123

### Own site
sm64coopdx runs its own forum/mod site with ratings and update logs. https://mods.sm64coopdx.com/mods/character-movesets.191/updates

---

## 7. Comparison table

| | N64Recomp / Zelda64Recomp | Unleashed Recompiled | SoH / 2Ship | sm64coopdx | DS Kart Recompiled | gbarecomp (MKSC) |
|---|---|---|---|---|---|---|
| Kind | Static recomp (MIPS to C) | Static recomp (PPC to C++) | Decomp port | Decomp port | Static recomp (ARM to C) | Static recomp (ARM/Thumb to C++) |
| Ships | Prebuilt exe; ROM gives assets | Prebuilt exe; user dump | Prebuilt exe; extracts O2R from ROM | Source; user builds w/ ROM | Launcher only; builds locally from ROM | Tool + sanitized packages; ROM-derived code stays local |
| ROM check | SHA-1, US only | Region/integrity checks | SHA-1 list | sha1 files | Game code AMCP | SHA-1 per revision |
| Renderer / UI | RT64 + RmlUi | plume + ImGui *(not verified)* | LUS + ImGui | SDL/GL | Custom (CPU 3D, GPU opt.), ImGui | Custom + recomp-ui (ImGui) |
| Code mods | Yes: C to MIPS, recompiled at load; patch/hook/event/export | No (planned); built-in codes | In-tree enhancements (CVars) + GameInteractor; libtcc scripts | Lua scripting | No | Native features under trusted plugin IDs |
| Asset mods | RT64 texture packs (.rtz) | HMM file replacement | .otr/.o2r in mods/ with load order | DynOS packs | Not stated | `.gbamod` data |
| Config | Typed options in mod.toml, auto UI | HMM | CVars / ImGui | `hook_mod_menu_*` | ini + F1 menu | Launcher |
| Mod manager | In-game menu, drag-drop, toggle | External HMM | In-game load-order menu | In-game | n/a | Launcher |
| Distribution | Thunderstore (manual DL) | GameBanana (1-click) | GitHub/Discord | Own site + itch | n/a | n/a |
| Build size/time | n.s. | 6–10 GiB install | n.s. | n.s. | ~5 min/8 cores, 200 MB DL, 1.5 GB | n.s. |
| License | MIT tool / GPL-3 runtime and game | GPL-3 (tool MIT) | LUS MIT; 2Ship CC0 | source-available | MIT | PolyForm NC |

(n.s. = not stated in the sources fetched)

---

## 8. Recommendations for MKDSrust

### 8.1 Mod API shape (three tiers, one manifest)

**Tier A: Luau scripts (default, sandboxed, cross-platform).**
- Embed via `mlua` with the `luau` feature. Use `Lua::sandbox(true)` for read-only globals and per-script environments, and `set_interrupt` as a watchdog against runaway scripts. https://docs.rs/mlua/latest/mlua/struct.Lua.html, https://luau.org/sandbox
- Expose a **curated semantic event bus** in the SoH GameInteractor style, not raw addresses:
  - `events.on("race_start", fn)`, `on("lap_complete", fn(kart, lap))`, `on("item_roll", fn(ctx) -> item?)`, `on("kart_update", fn(kart))`, `on("frame")`, `on("menu_open")`.
  - Make the payloads typed userdata over the real structs (names and types from the decomp dictionary).
- Add **decision hooks** in the style of SoH's `OnVanillaBehavior`: `should.on("cpu_uses_item", fn(ctx) -> bool)`. **Fix the polarity once:** always return `nil` for vanilla, `false` to cancel and `true` to force. This avoids sm64coopdx's inconsistent `ALLOW_*` returns. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/guides/hooks.md
- **Function-level hooks** for anything not yet covered: `hooks.before("RaceManager_Update", fn)` and `hooks.after(...)`. These are implemented in the generated Rust as a cheap "any hooks registered?" check at the entry/return of each named function. The named function list comes from the dictionary, so hook targets are stable symbol names, not addresses (same idea as N64Recomp's name-based `RECOMP_HOOK`). https://hackmd.io/@Wiseguy/HyhUXwiV0
- Provide inter-mod `exports` and `imports`, and mod-declared events (`events.declare("mymod:boost")`), mirroring `RECOMP_EXPORT/IMPORT/DECLARE_EVENT/CALLBACK`.
- Auto-generate the Luau type definitions (`.d.luau`) and docs from the binding layer, with a VS Code / luau-lsp setup page. This is the sm64coopdx lesson, and it is crucial for LLM-assisted modding. https://github.com/coop-deluxe/sm64coopdx/blob/main/docs/lua/lua.md

**Tier B: native Rust function replacement (power users, readable source).**
- Since MKDSrust emits readable Rust, a "code mod" can be a Rust crate that implements a replacement for a named function and is linked at build time. Because builds are local, this works naturally, which is a key advantage over N64Recomp.
- Follow N64Recomp's rules: replacing the same function in two mods is a hard conflict; hooks always compose; force-replace is gated. https://hackmd.io/@Wiseguy/HyhUXwiV0
- Prefer build-time linking over loading arbitrary DLLs, following gbarecomp's "no arbitrary DLLs, trusted plugin IDs" stance. This avoids per-platform binaries and malware vectors. https://github.com/mstan/gbarecomp

**Tier C: data/asset mods.**
- Use a virtual filesystem overlay over the NitroFS (file replacement by path: NARC/NKM/SDAT/textures), with explicit load order and per-file conflict reporting. This is the SoH/LUS archive overlay model with a drag-sortable order. https://github.com/Kenix3/libultraship, https://giteapons.duckdns.org/nickpons666/Shiip-of-Hakinian-Espanol/src/commit/dc5e9686c0a5c1b4f1cdf285ad8bf581be6233e1
- First-class "track pack" support for existing MKDS custom tracks (the Wiicompiled + Retro Rewind lesson). https://www.tomshardware.com/video-games/retro-gaming/mario-kart-wii-recompiled-for-pc-using-ai-with-4k-potential-and-uncapped-frame-rates-first-static-recompilation-of-a-wii-game-supports-over-200-tracks-thanks-to-retro-rewind-compatibility

**Config**: declare typed options in the manifest (bool, enum, int/float with min/max/step, string, keybind). The launcher renders them automatically, as N64Recomp's `mod.toml` config options do and like `hook_mod_menu_*`. Scripts read them with `config.get("id")` and receive `on("config_changed")`. https://raw.githubusercontent.com/Zelda64Recomp/MMRecompModTemplate/main/mod.toml

### 8.2 Mod package format / manifest

**Single zip with extension `.mkmod`**, Thunderstore-compatible at the root, so the same file uploads to Thunderstore unchanged (https://wiki.thunderstore.io/mods/creating-a-package):

```
my_mod.mkmod (zip)
  manifest.json      # Thunderstore fields (name, version_number, description, website_url, dependencies)
  icon.png           # 256x256
  README.md
  mod.toml           # MKDSrust-specific, authoritative
  scripts/main.luau
  files/...          # NitroFS overlay (path-mirrored)
  native/            # optional Rust crate source for Tier B (compiled locally, never prebuilt DLLs)
```

`mod.toml` sketch (fields drawn from N64Recomp, gbarecomp and Thunderstore):

```toml
[mod]
id = "author.boost_tweaks"   # reverse-DNS-ish, stable
version = "1.2.0"            # semver
name = "Boost Tweaks"
authors = ["..."]
api = "1.x"                  # MKDSrust mod API semver range
games = ["AMCE", "AMCP"]     # + optional rom_sha1 list per revision
[dependencies]
"author.kart_lib" = ">=0.3, <0.4"
[optional_dependencies]
[incompatible]
"other.mod" = "*"
[hooks]                      # static declaration -> conflict check before load
replace = ["Item_GetRoll"]   # exclusive
before  = ["Kart_Update"]
[[config]]
id = "boost_mult"; type = "float"; min = 0.5; max = 3.0; step = 0.1; default = 1.0
[permissions]                # Luau capabilities, shown to user
net = false; fs = "mod-dir"
```

- Static declaration of replacements lets the manager flag conflicts *before* launching, as N64Recomp does at load. https://hackmd.io/fMDiGEJ9TBSjomuZZOgzNg
- Use semver **ranges** internally, and translate to Thunderstore's exact `Team-Name-x.y.z` strings when publishing. https://wiki.thunderstore.io/mods/creating-a-package
- Check ROM, deps and conflicts before activation, and never mutate the user's ROM (gbarecomp). https://github.com/mstan/gbarecomp

### 8.3 Mod manager UX (in the launcher)
- Drag-and-drop install onto the window plus an "Install mod…" button, with toggles and per-mod config pages (Zelda64Recomp). https://github.com/Zelda64Recomp/Zelda64Recomp
- Draggable load order for asset overlays, with order persisted and stale entries pruned (SoH). https://giteapons.duckdns.org/nickpons666/Shiip-of-Hakinian-Espanol/src/commit/dc5e9686c0a5c1b4f1cdf285ad8bf581be6233e1
- **Profiles** and shareable profile codes (a lockfile of mod ids, versions and config) (r2modman). https://github.com/ebkr/r2modmanPlus
- A conflict and dependency panel that explains in plain language which mods replace the same function or file. This also matches the owner's feedback to "organise by game meaning": group mods by Karts / Tracks / Items / UI / Rules.
- Register an `mkdsrust://install?...` URL scheme so 1-click buttons on Thunderstore or GameBanana work (HMM pattern). https://flathub.org/fa/apps/io.github.hedge_dev.hedgemodmanager, https://mods.vintagestory.at/show/mod/8123
- Portable mode via a marker file (`portable.txt`). https://github.com/Zelda64Recomp/Zelda64Recomp

### 8.4 Distribution
1. **Thunderstore first** (free hosting, dependency graph, API, r2modman profiles). Request an `mkdsrust` community once a handful of mods exist, since they want mods ready to upload. https://wiki.thunderstore.io/ecosystem/adding-a-new-game
2. **GameBanana second**, for track and skin authors, who already live there, via 1-click links. https://github.com/s0nought/gb-api-v11
3. **Own site later, as an index only**, pulling from the Thunderstore API. This fits the memory note that the mod site is hidden/deferred.
4. Never host ROMs or patched `.nds` files. Ship mods as diffs and overlays only (the CTGP-N legality debate shows the risk). https://xblade720.itch.io/ctgp-nitro/community

### 8.5 Build-from-ROM flow (light, fast, modular)
MKDSrust's policy is to ship only its own code and generate ROM-derived code locally. That puts it in the DS Kart / PH / gbarecomp camp (https://github.com/AlonsoTlol/DS-Kart-Recompiled, https://github.com/mstan/gbarecomp). Recommended flow:
1. A small launcher (egui) is the only mandatory download.
2. **ROM picker:** check the SHA-1 or game code against a supported list, with friendly errors for trimmed, encrypted or modified dumps and wrong regions (Unleashed's integrity checks; SoH's supportedHashes). https://github.com/hedge-dev/UnleashedRecomp, https://github.com/HarbourMasters/Shipwright
3. **Components as optional downloads**, each versioned: runtime (prebuilt, contains no ROM-derived code), renderer variants, netplay, readable-source pack (the generator plus dictionary), Toolbox, Luau runtime. DS Kart's ~200 MB first-run download of Python and a compiler is the benchmark to beat. Ship a pinned minimal toolchain, or better, avoid needing one for the default path (see 4). https://github.com/AlonsoTlol/DS-Kart-Recompiled
4. **Fast path:** split the build so that only the ROM-derived generated Rust is compiled locally and linked against a prebuilt runtime `.rlib`/staticlib. Cache per (ROM sha1, generator version, mod set) and rebuild only modules touched by Tier B mods. This mirrors gbarecomp's sharded C++ + `.lib` + persistent cache. https://github.com/mstan/gbarecomp
5. **Version-lock generated output:** store the generator version in the build and offer regeneration on update (SoH's OTR version check). https://giteapons.duckdns.org/nickpons666/Shiip-of-Hakinian-Espanol/src/commit/7e9efeeadb80a48279cc5744e323b1844a8cda95
6. An "Update" button that keeps the ROM, settings and saves (DS Kart). Print "don't share your build" guidance (PH recomp). https://github.com/dguillot-gh/phantom-hourglass-recomp

### 8.6 Licenses of reusable pieces (MKDSrust is GPL-3.0)

| Piece | License | Use for MKDSrust |
|---|---|---|
| N64Recomp (tool, mod section-name design) | MIT | Design reference; code reusable https://github.com/N64Recomp/N64Recomp |
| N64ModernRuntime / librecomp (mod loader impl) | GPL-3.0 | Compatible; reference for mod loader logic https://github.com/N64Recomp/N64ModernRuntime |
| MMRecompModTemplate (`modding.h`, `mod.toml`) | CC0-1.0 | Freely reusable https://github.com/Zelda64Recomp/MMRecompModTemplate |
| RT64 | MIT | N64-specific; reference only https://github.com/rt64/rt64 |
| Zelda64Recomp / MK64Recomp | GPL-3.0 | Compatible (menus, mod menu UX) https://github.com/Zelda64Recomp/Zelda64Recomp |
| XenonRecomp | MIT | Midasm-hook config idea https://github.com/hedge-dev/XenonRecomp |
| Unleashed Recompiled | GPL-3.0 | Installer/verification flow https://github.com/hedge-dev/UnleashedRecomp |
| libultraship | MIT (bundled libtcc LGPL-2.1) | Archive overlay model https://github.com/Kenix3/libultraship |
| 2Ship2Harkinian | CC0-1.0 | Freely reusable https://github.com/HarbourMasters/2ship2harkinian |
| ds-decomp (Rust) | MIT | Directly reusable crates for ROM extract/delink https://github.com/AetiasHax/ds-decomp |
| ndskit | MIT | Overlay-residency (CRC on dirty page) technique https://github.com/vs-sr-dev/ndskit |
| DS Kart Recompiled | MIT | Compatible; check before borrowing (direct competitor) https://github.com/AlonsoTlol/DS-Kart-Recompiled/blob/main/LICENSE |
| Phantom Hourglass recomp / ntr-runtime | GPL-3.0 | Compatible https://github.com/dguillot-gh/phantom-hourglass-recomp |
| gbarecomp / recomp-ui | PolyForm Noncommercial | **Not GPL-compatible: ideas only, no code** https://github.com/mstan/gbarecomp |
| r2modman | MIT | Profile/export UX reference https://github.com/ebkr/r2modmanPlus |
| mlua (+ Luau) | MIT | Luau embedding https://docs.rs/mlua/latest/mlua/struct.Lua.html |
| RmlUi | MIT | Only if moving off egui https://github.com/mikke89/RmlUi |
| sm64coopdx | "source-available" (no clear license) | **API design reference only** https://en.wikipedia.org/wiki/Super_Mario_64_Coop_Deluxe |

### 8.7 Open questions / follow-ups
- Locate the "tylzars" MKDS matching decomp: it was not found by search.
- Inspect DS Kart Recompiled's code layout and runtime (MIT) to benchmark build time and size against MKDSrust. Do not run its binaries.
- Decide early whether hook dispatch overhead on every named function is acceptable. Alternative: only functions listed in the active mods' `[hooks]` get dispatch, determined at local build time. This is possible only because MKDSrust builds locally.
