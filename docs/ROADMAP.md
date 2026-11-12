# MKDSrust roadmap

**MKDSrust is the ultimate modding tool for Mario Kart DS.** Point it at your own cartridge dump and it builds, on your
computer: readable Rust source of the whole game, its assets in editable form, a native PC game, and mods that run on
PC and, where possible, back on a real DS, DSi or 3DS.

MKDSrust is a build script, not a distribution. It ships only its own code. Nintendo's code and assets never leave
your computer: everything game-derived is generated locally from your ROM.

_Status: draft of 2026-11-11, based on four research reports in [docs/research/](research/):
[community tools](research/community.md), [DS code injection](research/ds_injection.md),
[recompilation projects and their mod systems](research/recomp_mods.md), [app architecture and format libraries](research/tauri_formats.md)._

## Principles

1. **Your ROM, your machine.** We ship our tools; the game comes from your dump. Nothing derived from Nintendo is
   committed to this repository or published by it.
2. **Readable first.** The generated Rust should read like source code: real names, real types, structured control
   flow, no simulated CPU. Readability is what makes deep mods possible, for people and for the AI tools they use.
3. **Proven, not hoped.** Every change to how the game is generated must play a reference race identically (frame for
   frame in accuracy mode). Accuracy is our foundation.
4. **Pay only for what you use.** Everything heavy is an optional component with its size and time shown before you
   choose it. Nobody downloads a compiler they don't need.
5. **Everything everywhere, honestly labelled.** Mods target PC, DS hardware, or both, and the launcher says which.
6. **Learn from everyone.** We study every MKDS tool and recompilation project, reuse code where licenses allow, and
   credit it in [CREDITS.md](../CREDITS.md).

## The user's journey

1. Download the launcher (a few MB). Choose your ROM; it is checked first, always.
2. On the **Build** page, pick what you want, each with its download size and time:
   - **Source and assets** (default): readable Rust and extracted assets in about a minute, no compiler needed.
   - **Playable PC game**: compiles the native game (needs the Rust toolchain, downloaded once).
   - **Toolbox**: the asset editors, as a tab in the launcher.
   - **DS export**: the ARM toolchain for putting mods on real hardware.
3. Play on PC. Import a save from an emulator, flashcart or earlier install; or unlock everything for testing (on a copy
   of your save).
4. Make a mod: edit assets in the Toolbox tab, change code in the readable source, or install community mods.
5. Play it on PC. The launcher checks whether the mod also works on a DS.
6. Export it: a patched ROM or a shareable patch for DS, DSi, 3DS and emulators.

## Architecture

| Part | What it is | Ships as |
|---|---|---|
| Launcher | Tauri app (planned; egui today): ROM, Build, Play, Mods, Toolbox, Export tabs | Download (small) |
| Generator | ROM machine code to readable Rust, names and types from our dictionary | Prebuilt, inside the launcher |
| Runtime | Native renderer, audio, input, file system; decoupling the game from DS hardware | Our source, compiled on the user's PC (prebuilt later) |
| Toolbox | Asset editors (models, tracks, textures, text, music) | Optional component, a launcher tab |
| Mod API | Hooks on named game functions and events; one mod source, two backends | Part of the source |
| PC backend | Mod compiled into the native game | With the game build |
| DS backend | Same mod compiled for the DS's ARM9 and injected into your ROM | Optional component |

## What the research changed

- **Code mods on real hardware are realistic, in DS mode.** The community injects compiled code into retail ROMs
  (NCPatcher, GPL-3.0; Garhoogin's RuntimeCodeModules for MKDS, public domain). Rust can target the DS's ARM9
  (`thumbv5te-none-eabi`, nightly). Turning MKDS into a DSi-mode game for its 16 MB is not realistic, so DSi and 3DS
  run mods in DS mode (flashcarts, TWiLight Menu++/nds-bootstrap), within the game's 4 MB budget.
- **Mod API shape**, from the best existing systems:
  - N64Recomp/Zelda64Recomp: hooks declared in code (replace, before, after, events, exports between mods), a
    `mod.toml` with dependencies and typed settings, conflicts detected before launch.
  - sm64coopdx: event-driven Lua with automatic settings menus and docs generated from the bindings.
  - Ship of Harkinian: "decision hooks" that change one game decision without replacing a whole function.
- **Distribution:** packages that are also valid Thunderstore packages; Thunderstore first, GameBanana second,
  mkdsrust.com as an index. Never host ROMs or patched `.nds` files. DS exports are BPS/xdelta patches against a
  hash-checked ROM (IPS cannot address MKDS's 32 MB).
- **Interoperate, don't rebuild:** Mario Kart Toolbox (MIT) is the maintained track editor (NKM/KCL); apicula (0BSD,
  Rust) converts DS models; ds-rom (MIT, Rust) rebuilds matching ROMs and is the base for our own injector.
- **We fill real gaps:** public symbol work targets the European version, so USA names plus an EU↔USA address map are
  new; no public item-distribution or AI mods exist; "200cc" today is a crude speed code; hard-coded tables (music,
  weather, cups) need data files. These become first-party example mods.
- **The other port:** DS Kart Recompiled (EU only, MIT) builds in about 5 minutes on 8 cores with a 200 MB download.
  Our edges: a mod system, readable named source, USA (and later EU), and modular downloads.
- **Launcher stack:** one Tauri backend with our format crates built in; on-demand tabs are downloaded frontend
  bundles served through a verifying custom protocol; toolchains are downloaded components; updates are signed
  (tauri-plugin-updater 2.12+, signed versions required).

## Milestones

| # | Milestone | Contents | Status |
|---|---|---|---|
| 1 | Lighter builds (0.7.1) | Generator on all cores, prebuilt generator, compile jobs by memory, installer archives removed, readable-source toggle, no console windows. First build 6 min → 4 min 18 s, game folder 342 → 157 MB, race identical | Done |
| 2 | Build page and decomp-only path | Obvious component toggles with sizes; source and assets without a compiler | Next |
| 3 | Saves | Import save (shows its progress; melonDS/flashcart .sav, DeSmuME .dsv), export, unlock all on a separate test save; per-user data no longer copied into portable installs | Done |
| 4 | One app | Launcher moves to Tauri + React; Toolbox becomes an on-demand tab | Planned |
| 5 | Mod API with two backends | Hooks on named functions (replace/before/after/decision/events), `mod.toml`, conflict check; PC build; DS backend first via NCPatcher with generated C stubs, then our own injector on ds-rom; DS-compatibility check (code size, heap, hardware limits); BPS/xdelta export | Planned |
| 5b | First-party example mods | Real 200cc, item-distribution editor, alt skins, speedrun quality-of-life, music/cup tables as data | Planned |
| 6 | Community | Mod sharing, templates, docs for modders and their AI tools | Planned |
| ∞ | Readability and decoupling | More functions as plain, typed, structured Rust without the simulated DS; smaller, faster builds as a result | Ongoing |

## Open questions

- Measure on AMCE: where the static code ends, free heap during a race, which overlays hold race code (for DS mods).
- Contact the scene early (Ermelber, Gericom, Garhoogin, HaroohiePals) about interoperability and permissions for
  unlicensed tools.

- Output language for readable functions: measure a C backend against Rust on one game area before deciding.
- Mod distribution: own site, GameBanana, Thunderstore, or several.
- License: GPL-3.0 today; revisit if a component we want to reuse needs it.
