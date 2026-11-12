# DS code injection, compiling Rust/C for the DS, and distributing patches: research for MKDSrust

Date: 2026-11-11. Scope: "one mod source, two outputs". The PC output links into the native MKDSrust build. The DS output is an ARMv5TE blob injected into the user's own MKDS USA rev 0 (AMCE) ROM and shipped as a patch.
How this was checked: web pages plus GitHub API metadata (license SPDX id, last push). Nothing was downloaded or run, except that NCPatcher's source was read through the GitHub API to see how it finds hooks.

---

## 0. TL;DR

- **The mature path for injecting C/C++ is NCPatcher (GPL-3.0, active, last commit 2026-04-11) plus an `arm-none-eabi` GCC.** It is the standard for NSMB DS and is credited by the MKDS RE community (XorTroll's mkds-re). It finds arenaLo itself, places new code there or in a new or replaced overlay, and writes ARM/Thumb jump, call and hook branches with bridges. It can BLZ-compress the output. Sources: https://github.com/TheGameratorT/NCPatcher, https://github.com/XorTroll/mkds-re
- **Rust can build for the DS ARM9 today, but the target is Tier 3.** The targets are `armv5te-none-eabi` and `thumbv5te-none-eabi`: `core` and `alloc` only, nightly with `-Zbuild-std`. Per-function ARM/Thumb choice is stable through `#[instruction_set(arm::a32/t32)]`. Sources: https://doc.rust-lang.org/nightly/rustc/platform-support.html, https://doc.rust-lang.org/stable/rustc/platform-support/armv5te-none-eabi.html, https://doc.rust-lang.org/reference/attributes/codegen.html
- **NCPatcher cannot take Rust objects directly.** It only compiles `.c`, `.cpp` and `.s` files itself, and it scans only *those* objects for `.ncp_*` hook sections (checked in its source: `objmaker.cpp` and `patchmaker.cpp::gatherInfoFromObjects`). Rust can emit the same section names with `#[link_section]`, but NCPatcher would need a small patch or a generated C shim layer to see them.
- **Recommendation:** MKDSrust owns a small hook manifest (function name → AMCE address, ARM or Thumb, hook kind) and generates two things from it:
  - (a) Rust glue for the PC build;
  - (b) for the DS, *either* NCPatcher `ncp_*` C shims around a Rust `staticlib` (fastest route to a prototype), *or* our own MIT/GPL injector in Rust built on AetiasHax's `ds-rom` (MIT; handles LZ77/BLZ, overlays and autoloads) that does arenaLo/autoload placement plus branch writing.
  
  Ship results as **BPS** (or xdelta3) patches against a hash-verified AMCE ROM. **IPS cannot address past 16 MiB, and MKDS is a 32 MiB ROM.**
- **DSi/3DS:** MKDS is a plain NTR (DS) title, so a patched ROM runs in DS mode (4 MB, about 67 MHz) on flashcarts, nds-bootstrap and TWiLight Menu++. The main risk is nds-bootstrap's **heap shrink** and its own RAM use colliding with injected code that is not part of the static image. Code placed by raising arenaLo or by a proper overlay is the safe choice. Sources: https://manual.ds-homebrew.com/en/nds-bootstrap, https://git.randommeaninglesscharacters.com/rvtr/wiki/raw/commit/4389e9f167672ab6fb87641982c64170f2330747/pages/_en-US/nds-bootstrap/faq.md

---

## 1. Tool table

"Active" is based on the GitHub `pushed_at` / last-commit date read 2026-11-11 through the GitHub API.

| Tool | URL | Purpose | License | Active? | Fit for MKDSrust |
|---|---|---|---|---|---|
| **NCPatcher** | https://github.com/TheGameratorT/NCPatcher | Universal DS code maker/patcher: compiles C/C++/asm, links against a symbols file, inserts at arenaLo or into an overlay (append/replace/create), writes jump/call/hook/over patches, ARM and Thumb, runtime patches (`ncprt_*`), BLZ compress | GPL-3.0 (LICENSE.txt) | Yes (last commit 2026-04-11) | **High.** Best existing injector. Invoke as an external tool; vendoring is fine under GPL-3.0 since MKDSrust is GPL-3.0 |
| **Fireflower** (+ nds-extract / nds-build) | https://github.com/MammaMiaTeam/Fireflower | Older DS patching toolchain: hooks `hook`/`rlnk`/`safe`/`over`, new autoload placed in the old heap area, `thumb` keyword | BSD-3-Clause | No (last commit 2022-01-16; README says unfinished) | Low as a patcher. Its nds-extract/nds-build are still used with NCPatcher. Good reference for design |
| **NSMB-Code-Template** | https://github.com/MammaMiaTeam/NSMB-Code-Template | Reference project layout for NCPatcher: `arm9.json`, `ncpatcher.json`, Arm GNU Toolchain GCC ≥ 12, C++23, insertion through NSMBe, ndspy script or nds-extract/build | No license file (GitHub reports none) | Yes (2026-08-24) | Template to copy *ideas* from. Do not copy files (no license). It also relies on the Nintendo SDK headers we must not ship |
| **NSMB Editor (NSMBe)** "Make and insert" | https://github.com/Dirbaio/NSMB-Editor, https://nsmbhd.net/post/63405/ | Legacy GUI code insertion: `arenaoffs.txt` (the arenaLo address), `hook_02xxxxxx` / `nsub_` / `repl_` naming, reads `newcode.sym` | GPL-3 (custom notes on derivative works) | No (2022) | Historical. The naming convention is worth knowing |
| **EveryFileExplorer ASMHack** | https://git.randommeaninglesscharacters.com/Gericom/EveryFileExplorer/src/branch/master/NDS/Nitro/ASMHack.cs | Clearest small reference for the arenaLo technique: read ArenaLo, compile at that address, bump ArenaLo by the code length, add an autoload entry | No license file | No (2021) | Reference only |
| **RuntimeCodeModules (RCM)** | https://github.com/Garhoogin/RuntimeCodeModules | **Mario Kart DS** relocatable code modules loaded at runtime (race start and some menus) from `code.rcm` in an archive. Hook table has AREPL (BL), ANSUB (B), TREPL (BLX), TNSUB (8-byte), DATA32/16, LOAD/UNLOAD. Relocations ABS32/CALL/JUMP24/BASE_ABS | Unlicense (public domain) | No (2023-05) | **Very relevant design precedent.** It is MKDS-specific, freely reusable and shows "plugins as data files". Region support is not documented, so check it against AMCE |
| **mkds-re / mkdsdecomp** (XorTroll) | https://github.com/XorTroll/mkds-re | MKDS **EU (AMCP)** RE, ASM-hack examples with generated headers and symbols, credits NCPatcher | MIT | Low (2025-01) | Useful cross-reference. Addresses are EU, not AMCE |
| **dsd / ds-decomp** (AetiasHax) | https://github.com/AetiasHax/ds-decomp | DS decomp toolkit: `rom extract/build/config`, `init`, `delink` to ELF, `lcf` for mwldarm, objdiff integration, symbol and relocation configs | MIT | Yes (2026-10-24) | High for the decomp side (symbols and relocations). Not an injector |
| **ds-rom** (AetiasHax) | https://github.com/AetiasHax/ds-rom | Rust lib + `dsrom` CLI to extract and **matching-rebuild** DS and DSi-enhanced ROMs. Has `compress/lz77.rs`, `huffman.rs`, `rom/autoload.rs`, `overlay_table.rs` | MIT | Yes (2026-10-25) | **High.** Rust-native ROM I/O and compression to build our own injector on. MIT, so it can be vendored |
| **ndspy** | https://ndspy.readthedocs.io/en/latest/api/codeCompression.html | Python lib: ROM, overlays, `codeCompression` (BLZ, `isArm9` flag), LZ10 | GPL-3.0 | Yes (2026-08-29) | Good scripting and reference implementation of BLZ. Used by the NSMB template's `insert_code.py` |
| **armips** | https://github.com/Kingcom/armips | Assembler for patching: `.nds` (ARM9), `.open`/`.org`/`.headersize`, `.importobj`/`.importlib` to link ELF objects at a position, `.area`/`.autoregion` | MIT | Yes (2026-08-01) | Medium. A good fallback for hand-written hooks or for dropping a Rust `.o` at an address |
| **ndstool** | https://github.com/devkitPro/ndstool, https://github.com/blocksds/ndstool | Build and unpack `.nds`, header fix | GPL-3.0 | devkitPro 2024-11; BlocksDS fork 2026-11 | Medium. Invoke as a separate binary (fine) |
| **Kamek** (Wii) | https://github.com/Treeki/Kamek | Wii/GameCube code injection engine (the "Kamek" idea: hooks declared in code, linker resolves game symbols) | MIT | Yes (2026-11-10) | Design reference only (PowerPC) |
| **Magikoopa** (3DS) | https://github.com/RicBent/Magikoopa | 3DS code injection (NSMB2) | GPL-3.0 | No (2019) | Reference only |
| **devkitARM / libnds (devkitPro)** | https://github.com/devkitPro/libnds | Classic homebrew SDK; GCC toolchain | libnds is Zlib (per BlocksDS docs); GitHub reports NOASSERTION for the devkitPro repo | Yes | Not needed. Injection does not use libnds. BlocksDS notes much devkitPro-distributed code lacks a license: https://www.skylyrac.net/p/2023-08-18-blocksds-sdk/ |
| **BlocksDS** | https://blocksds.skylyrac.net/, https://codeberg.org/blocksds/sdk | Modern FOSS DS SDK on the Wonderful Toolchain (GCC). libnds Zlib, crts MPL-2.0, Maxmod ISC, DSWiFi MIT, picolibc MIT/BSD, libgcc GPL+runtime exception | Mixed FOSS (see §4) | Yes (moved to Codeberg, 2026) | Medium. Its GCC is a fine `arm-none-eabi` for NCPatcher. Its libnds and crts are **not** wanted inside a retail-game patch |
| **Arm GNU Toolchain** (`arm-none-eabi`) | https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads | Official prebuilt bare-metal GCC/binutils | GPL/LGPL components (https://launchpad.net/gcc-arm-embedded) | Yes | **High.** The toolchain the NSMB template uses. Simplest to require from users |
| **Rust `armv5te-none-eabi` / `thumbv5te-none-eabi`** | https://doc.rust-lang.org/stable/rustc/platform-support/armv5te-none-eabi.html | rustc bare-metal targets for ARMv5TE | MIT/Apache-2.0 (Rust) | Tier 3 (maintainer @QuinnPainter) | **High, with nightly risk** |
| **ironds** | https://github.com/QuinnPainter/ironds | Rust homebrew framework for the DS (has arm9/arm7 linker scripts) | Zlib | No (2023-04) | Reference for linker scripts and target config |
| **nds-rs** | https://github.com/BlueTheDuck/nds-rs | Safe Rust abstractions over libnds | None stated | Some (2026-03) | Low (homebrew, not injection) |
| **cargo-nds / libnds-sys** | https://git.randommeaninglesscharacters.com/SeleDreams/cargo-nds | Cargo subcommand using a custom `armv5te-nintendo-ds.json` target and the BlocksDS toolchain | n/a | ? | Low |
| **SquidTWL** | https://squidtwl.readthedocs.io/en/latest/guides/getting-started.html | Rust DS/DSi homebrew: `.cargo/config.toml` `target = "armv5te-none-eabi"`, nightly build-std | n/a | ? | Reference for cargo config |
| **dsnative** | https://github.com/owenqyzhang/dsnative | Ahead-of-time translates DS ARM code to C | MIT | Yes (2026-11-10) | Relevant to the PC side, not injection |
| **nds-bootstrap** | https://github.com/DS-Homebrew/nds-bootstrap | Boots DS/DSi ROMs from SD on DSi/3DS (and flashcarts) by patching the game's card functions | GPL-3.0 | Yes (2026-11-07) | Must-test runtime, not a dependency |
| **TWiLight Menu++** | https://github.com/DS-Homebrew/TWiLightMenu | DSi/3DS/flashcart menu that launches through nds-bootstrap | GPL-3.0 | Yes (2026-11-12) | Must-test runtime |
| **Flips** (Floating IPS) | https://github.com/Alcaro/Flips | Create and apply IPS/BPS | GPL-3.0 | **Archived 2025-05-21** | OK to invoke. For in-app use, write or vendor a BPS lib instead |
| **xdelta3** | https://github.com/jmacd/xdelta | VCDIFF delta patches | Apache-2.0 (3.x) | Yes | Good alternative format, permissive |

---

## 2. Injection techniques and how each tool does them

### 2.1 Where new code lives in RAM

1. **Raise arenaLo and add an autoload (static, the most robust).**
   - In a Nitro SDK game, `OS_GetInitArenaLo` returns the end of the static main binary, and the heap begins there. NCPatcher's `arenaLo` key is "the address of the value holding the address end of the main binary code in memory, usually loaded by the first LDR in `OS_GetInitArenaLo`". If `arenaLo` is not given, NCPatcher searches for it automatically for ARM9 (`arenalofinder.cpp`). Sources: https://github.com/TheGameratorT/NCPatcher, and its source at `source/patch/patchmaker.cpp`.
   - EveryFileExplorer shows the whole recipe in a few lines: read ArenaLo, compile there, `ArenaLo += len`, `Arm9.AddAutoLoadEntry(ArenaLo, NewCode)`. Source: https://git.randommeaninglesscharacters.com/Gericom/EveryFileExplorer/src/branch/master/NDS/Nitro/ASMHack.cs
   - Fireflower does the same thing: it adds "a new autoload region that places the new code in the former heap area". Source: https://github.com/MammaMiaTeam/Fireflower
   - NSMBe uses `arenaoffs.txt`, which holds the address needed "to make free space in RAM to insert the new code". Source: https://nsmbhd.net/post/63405/
   - Cost: every KB we add is a KB less heap. MKDS's heap headroom has to be measured; this is an open item.
2. **New or replaced overlay.**
   - NCPatcher region modes are `append` (the only option for main), `replace` (clear an overlay and insert code) and `create` (a new overlay at a given `address`). Source: https://github.com/TheGameratorT/NCPatcher
   - The game must load a new overlay, so a stub calls `FS_LoadOverlay`. One translation hacker built Rust as an overlay, loaded it with an armips stub calling `FS_LoadOverlay`, moved the heap start, and it ran on hardware. Source: https://gbatemp.net/threads/the-first-time-i-use-rust-on-nds-game-translation-hacking.620045/ (the page returned 403 to the fetcher; the detail comes from the search-result summary).
3. **Unused high EWRAM ("free space").**
   - The Pokémon HGSS guide uses about 0x023C8000 to the end of EWRAM, which research found unused during gameplay. Source: https://blurosie.github.io/code_injection_nds.html
   - **This is risky for us.** nds-bootstrap and flashcart kernels use high RAM, and heap shrink moves the arena (§3).
4. **Heap-allocated, runtime-loaded modules.**
   - RCM for MKDS loads a relocatable `code.rcm` at race start or in menus, applies relocations, then hooks, and unhooks on unload. Source: https://github.com/Garhoogin/RuntimeCodeModules
   - Pokémon B2W2 / CTRMap uses a similar on-demand "DLL" model. Source: https://ds-pokemon-hacking.github.io/docs/universal/guides/code_injection
   - Advantages: mods are swappable data files and the static footprint is small. Cost: a loader must be injected statically first.
5. **Synthetic overlay** (Pokémon Gen IV): a generated file that extends existing code memory regions. Source: https://ds-pokemon-hacking.github.io/docs/generation-iv/guides/hgss-code_injection

### 2.2 Branch patching and ARM/Thumb interworking

The ARM946E-S is ARMv5TE, so `BLX imm`, `BLX reg` and `BX` are all available. Only Thumb-1 exists (no Thumb-2). Source: https://doc.rust-lang.org/stable/rustc/platform-support/armv5te-none-eabi.html

**NCPatcher** (from its README):

| Patch | Bytes written |
|---|---|
| ARM→ARM jump | 4 |
| ARM→Thumb jump | 4 + an 8-byte bridge (`LDR PC,[PC,#-4]` followed by target+1) |
| Thumb→ARM jump | 8 |
| call | 4 |
| hook | 4 + a 20-byte bridge (saves r0–r3 and r12, calls, restores, runs the displaced instruction, returns to addr+4) |

- Hooks only work *from* ARM code. For Thumb sites you use `ncp_tjump`/`ncp_tcall`/`ncp_thook` or address+1.
- Thumb-origin jumps overwrite 4 or 8 bytes, not 2.
- NCPatcher finds hooks through ELF **section names**. `ncp.h` defines `__attribute__((section(".ncp_" #opcode "_" #address)))`, for example `.ncp_jump_0x02012345` or `.ncp_hook_0x0211ABCD_ov12`, plus a `.ncp_set` pointer table for label-style patches. Checked in `runtime/ncp.h` through the GitHub API. Source: https://github.com/TheGameratorT/NCPatcher

**Fireflower:**
- `hook` writes `b`/`bx` with no saved instruction.
- `rlnk` writes `bl`/`blx`.
- `safe` writes a register-saving thunk that keeps the displaced instruction (deprecated).
- `over` overwrites bytes.
- Source: https://github.com/MammaMiaTeam/Fireflower

**NSMBe:**
- `hook_` writes a branch to a thunk that saves all registers and re-runs the replaced instruction.
- `nsub_` / `repl_` correspond to B / BL.
- Source: https://nsmbhd.net/post/63405/

**RCM (MKDS):**
- AREPL = BL, ANSUB = B, TREPL = BLX, TNSUB = an 8-byte Thumb sequence.
- Source: https://github.com/Garhoogin/RuntimeCodeModules

**HGSS method:**
- `ldr rN,=sym; bx rN` takes 8 or 10 bytes and clobbers rN.
- A full hook takes 0x1C bytes and saves LR.
- A Thumb `bl` pair reaches ±4 MB.
- Source: https://blurosie.github.io/code_injection_nds.html

**Implication for us:**
- The hook manifest must record, for each named game function, its address, its **instruction set** (ARM or Thumb, known from the decomp), whether it sits in **main or an overlay N**, and the **displaced instructions** (no PC-relative instructions may be relocated blindly).
- MKDS overlays are loaded at runtime, so patches into overlay code must be written into the overlay file. NCPatcher handles this with `_ovN`. A runtime patcher (`ncprt_*`, RCM) can apply them after the overlay loads.

### 2.3 Compression of arm9.bin and overlays

- Retail arm9 and overlays use "BLZ" (backwards LZ, decompressed in place from the end). ARM9 compression differs slightly from overlay compression (ndspy's `isArm9` flag). Source: https://ndspy.readthedocs.io/en/latest/api/codeCompression.html
- NCPatcher has a `compress` option per region (BLZ, in `source/blz.cpp`). Source: https://github.com/TheGameratorT/NCPatcher
- `ds-rom` has an `lz77` compressor and full autoload and overlay-table models, and aims for byte-matching rebuilds. Source: https://github.com/AetiasHax/ds-rom
- Pitfall: loaders mis-handle hacks whose arm9 was left decompressed. nds-bootstrap had to fix "attempting to decompress already-decompressed arm9 binary for certain ROM hacks". Keep the module-params compressed-size field consistent with the data, or recompress. Source: https://newreleases.io/project/github/DS-Homebrew/nds-bootstrap/release/v2.2.2 (and related release notes)

---

## 3. Compiling for the DS

### 3.1 C/C++

- Any `arm-none-eabi` GCC works: Arm GNU Toolchain, BlocksDS/Wonderful or devkitARM. Flags: `-march=armv5te -mtune=arm946e-s`, plus `-mthumb` or `-marm` as needed, and `-fno-exceptions -fno-rtti -ffreestanding`.
- The NSMB template requires GCC ≥ 12 for C++23. Source: https://github.com/MammaMiaTeam/NSMB-Code-Template
- **No SDK is needed inside the patch.** The injected code calls the game's own functions through a symbols file (`symbols9.x`, linker `INCLUDE`). The NSMB community converts *Nintendo's* SDK headers, which we must not ship. Our symbols and headers must be ours, generated from MKDSrust's decomp names. Source: https://github.com/MammaMiaTeam/NSMB-Code-Template

### 3.2 Rust

**Target status:**
- `armv5te-none-eabi` and `thumbv5te-none-eabi` are **Tier 3**, `no_std` only, with no host tools. Source: https://doc.rust-lang.org/nightly/rustc/platform-support.html
- Library support is `core` + `alloc`. The maintainer is @QuinnPainter (the ironds author). The targets support ARM/Thumb interworking; the thumb variant defaults to Thumb. Source: https://doc.rust-lang.org/stable/rustc/platform-support/armv5te-none-eabi.html
- Tier 3 means no prebuilt std and no CI guarantee, so you need nightly `cargo build -Zbuild-std=core[,alloc]`. Real-world example: https://matthewtejo.substack.com/p/making-nintendo-ds-roms-with-rust (Feb 2026: `staticlib`, linked with BlocksDS `arm-none-eabi-gcc` using `-mcpu=arm946e-s`; release builds preferred because debug builds are too large). Also https://squidtwl.readthedocs.io/en/latest/guides/getting-started.html

**Language features we rely on:**
- `#[instruction_set(arm::a32)]` and `#[instruction_set(arm::t32)]` are stable and supported on ARMv4T/ARMv5TE. The low bit of a function pointer follows the selected set. This is what we need to emit ARM entry thunks for hooks while keeping bulk code in Thumb for size. Source: https://doc.rust-lang.org/reference/attributes/codegen.html
- `#[link_section = ".ncp_jump_0x0200ABCD"]` with `#[no_mangle] extern "C"` reproduces NCPatcher's convention from Rust. Source: https://doc.rust-lang.org/reference/abi.html

**Practical constraints:**
- No native atomic CAS on ARMv5 (LDREX/STREX arrived in v6), so mod APIs should avoid `AtomicX::compare_exchange`.
- There is no FPU: floats are soft-float and slow. MKDS uses fixed-point fx32 anyway, so the mod API should expose the game's fixed-point types.
- Add `panic = "abort"` with a tiny panic handler, `opt-level = "z"`/`"s"`, LTO, and `codegen-units=1`.
- `alloc` needs a `GlobalAlloc` that forwards to the game's own heap functions (the NNS/OS heap) through symbols.

**Position-specific blobs:**
- Yes. Build a `staticlib` (or a relocatable `.o` with `--emit obj`), then link it with a linker script whose `MEMORY` origin is the arenaLo address. The game's symbols go in as `PROVIDE(sym = 0x02xxxxxx)` or `--defsym`.
- That is exactly what NCPatcher generates internally: an `INPUT(...)`, `MEMORY{...}` script, and `gcc -nostartfiles -Wl,--gc-sections,-T`. Checked in `patchmaker.cpp`.
- armips `.importobj` can also drop an ELF at a fixed `.org`. Source: https://github.com/Kingcom/armips
- RCM-style relocatable modules work too, but rustc/LLVM may emit relocation types beyond RCM's four (ABS32/CALL/JUMP24/BASE_ABS). Expect also `R_ARM_THM_CALL`, `R_ARM_PREL31` (unwind tables, so disable unwind) and `R_ARM_REL32`. The loader would need extending.

**NCPatcher + Rust gap (verified in source):**
- `objmaker.cpp` accepts only `{ ".c", ".cpp", ".s" }`.
- `gatherInfoFromObjects()` scans only objects NCPatcher compiled itself.
- A Rust `.a` passed through `ld_flags` will link, but any `.ncp_*` sections inside it are **not** discovered as patches, and `--gc-sections` may drop them.
- Workarounds:
  - (a) Generate a C shim per hook, `ncp_jump(0x...) void hook_x(...) { mkds_mod_hook_x(...); }`, and link the Rust staticlib through `ld_flags`.
  - (b) Upstream a small NCPatcher patch so that prebuilt `.o`/`.a` inputs are scanned, using `KEEP` on `.ncp_*`.
  - (c) Write our own injector.

**Prior art for "Rust injected into a retail DS game":**
- One documented case: a translation hack building Rust as an overlay, loaded by an armips stub calling `FS_LoadOverlay`, with the heap start moved. It ran on real hardware. https://gbatemp.net/threads/the-first-time-i-use-rust-on-nds-game-translation-hacking.620045/
- No public Rust + NCPatcher project was found.
- Rust *homebrew* exists: ironds (Zlib, includes `arm9_link.ld`), nds-rs, cargo-nds, SquidTWL. https://github.com/QuinnPainter/ironds, https://github.com/BlueTheDuck/nds-rs, https://git.randommeaninglesscharacters.com/SeleDreams/cargo-nds

---

## 4. DSi mode, 3DS, flashcarts and patch formats

**DSi-enhanced and DSi-mode ROMs:**
- Header unitcode at 0x012: 00h = NDS, 02h = NDS+DSi, 03h = DSi.
- The extended header holds MBK1–9 at 0x180–0x1AC and ARM9i/ARM7i offsets at 0x1C0 and 0x1D0.
- SHA1-HMACs cover the modules (ARM9 at 0x300, ARM7 at 0x314), and an RSA-1024 signature at 0xF80 covers header bytes 0–0xDFF.
- Source: https://dsibrew.org/wiki/DSi_cartridge_header
- DSi hardware: 16 MB RAM (DS: 4 MB) and a 133 MHz ARM9, downclocked in DS compatibility mode. https://emulation.gametechwiki.com/index.php/Special:MobileDiff/31753, https://www.dsibrew.org/wiki/Hardware
- **MKDS AMCE is an NTR title, so it runs in DS mode.** Converting it to TWL mode to get 16 MB is *not* realistic: it would need re-signed hashes, new MBK setup and SDK-level DSi init.
- nds-bootstrap offers a TWL clock speed option for DS games but does not adapt games to it. https://wiki.ds-homebrew.com/ds-index/dsi-twl-firm

**Running modded ROMs:**
- nds-bootstrap (GPL-3.0) launches DS ROMs from SD on DSi/3DS through TWiLight Menu++ (GPL-3.0). It also runs on flashcarts, including B4DS mode on DS Phat/Lite. https://github.com/DS-Homebrew/nds-bootstrap, https://manual.ds-homebrew.com/en/nds-bootstrap, https://www.gamebrew.org/wiki/TWiLight_Menu%2B%2B
- It patches the game's card-read functions to read from SD.
- It **shrinks the game's heap**, which breaks some games and is handled with a blacklist. The shrink size depends on the cardEngine size plus the FAT cache, up to 2.5 MB in some configurations.
  - Sources: https://git.randommeaninglesscharacters.com/rvtr/wiki/raw/commit/4389e9f167672ab6fb87641982c64170f2330747/pages/_en-US/nds-bootstrap/faq.md, https://newreleases.io/project/github/DS-Homebrew/nds-bootstrap/release/v0.57.0, https://newreleases.io/project/github/DS-Homebrew/nds-bootstrap/release/v0.36.0
  - So: injected code must live **inside the static image** (below arenaLo, or in an overlay) and never in "free" high EWRAM.
  - Modded MKDS must also be re-tested for heap exhaustion under heap shrink.
- Flashcarts: a patched `.nds` is just a ROM, and flashcart kernels run it as they run any dump. Compatibility "varies by card". In DS mode, games with Thumb ARM7 binaries may need a donor ROM. Source: the FAQ above.
- **The DSi DS-cart whitelist / RSA check** applies to physical carts launched by the DSi menu, not to SD loading through nds-bootstrap. https://dsibrew.org/wiki/DSi_system_flaws

**Patch formats:**

| Format | Limits / features | Source |
|---|---|---|
| IPS | 24-bit offsets, so a 16 MiB source limit, which is **too small for MKDS (32 MiB)**; no checksums | https://sneslab.net/wiki/Patch_Formats |
| BPS | Unlimited size; source, target and patch CRC32; metadata; handles moved data. Current community standard | https://sneslab.net/wiki/Patch_Formats, https://datacrystal.tcrf.net/wiki/Patch |
| xdelta3 / VCDIFF | Apache-2.0; checksums; good with moved data | https://github.com/jmacd/xdelta, https://emulation.gametechwiki.com/index.php/Apply_ROM_Hacks_and_Translations |

**Recommendation:** distribute **BPS** with a SHA-1 pre-check of AMCE rev 0. Our patch contains only our code, our hook bytes and the deltas to arm9/overlay tables (plus any BLZ recompression deltas).
- Caveat: recompression makes the delta larger and may embed reshuffled bytes of Nintendo code, because a compressed arm9 delta contains rearranged original data.
- Mitigation: emit the patched arm9/overlays *uncompressed* (with the header/module-params compressed size set to 0) so the BPS contains only new bytes plus copy ops. Verify that MKDS boots with a decompressed arm9 under nds-bootstrap, which had a bug with exactly this case that has since been fixed.

---

## 5. Licenses: what we can vendor or invoke

MKDSrust is GPL-3.0.

| Component | License | Vendor into MKDSrust? | Invoke as an external tool? | Ends up in the shipped DS patch? |
|---|---|---|---|---|
| NCPatcher | GPL-3.0 | Yes (compatible) | Yes | No (it is a tool). Its `ncprt.c` runtime is GPL-3.0; if linked into mods, the mod blob is GPL-3.0, which matches our license |
| Fireflower | BSD-3 | Yes, with notice | Yes | No |
| ds-rom / ds-decomp | MIT | Yes, with notice | Yes | No |
| ndspy | GPL-3.0 | Yes | Yes | No |
| armips | MIT | Yes | Yes | No |
| RuntimeCodeModules | Unlicense | Yes, freely (credit as courtesy) | n/a | Loader code could be adapted into our patch |
| ndstool | GPL-3.0 | Yes | Yes | No |
| Flips | GPL-3.0 (archived) | Yes | Yes | No |
| xdelta3 | Apache-2.0 | Yes (Apache-2.0 is GPL-3-compatible) | Yes | No |
| BlocksDS libnds / crts / Maxmod | Zlib / MPL-2.0 / ISC (https://blocksds.skylyrac.net/docs/guides/licenses/) | Not needed | n/a | Avoid. The patch should call game code, not libnds |
| libgcc (when GCC links mods) | GPL-3 + GCC Runtime Library Exception | n/a | n/a | Allowed; the exception covers it |
| Rust core/alloc/compiler_builtins | MIT/Apache-2.0 | n/a | n/a | Yes, fine |
| NSMB-Code-Template, EveryFileExplorer | No license | **No** (all rights reserved by default) | n/a | Ideas only |
| devkitPro libnds/tools | libnds Zlib; much other devkitPro content unlicensed (https://www.skylyrac.net/p/2023-08-18-blocksds-sdk/) | Avoid vendoring | Users may install it | No |
| Nintendo SDK headers/libs (Nitro SDK, NitroSystem) | Proprietary | **Never** | n/a | **Never.** Our headers come from our own decomp names |

---

## 6. Recommended architecture: "same mod source → PC build and DS patch"

```
mod crate (Rust, #![no_std] + alloc, depends on `mkds-mod-api`)
   │   #[mkds::hook(Kart::update_physics, kind = Around)]   // names, not addresses
   │   fn my_physics(orig: Orig, kart: &mut Kart) { ... orig(kart) ... }
   │
   ├── PC:  cargo build (x86_64-pc-windows-msvc) ──► linked into MKDSrust native build
   │        mkds-mod-api "pc" backend = function-pointer tables / inventory registry in the
   │        lifted/readable Rust; Kart etc. are the real #[repr(C)] structs.
   │
   └── DS:  cargo +nightly build -Zbuild-std=core,alloc --target thumbv5te-none-eabi
            mkds-mod-api "ds" backend = extern "C" decls resolved by symbols9.x (AMCE addresses)
            proc-macro emits: #[instruction_set(arm::a32)] entry thunk + hook-manifest entry
                     │
                     ▼
            mkds-inject (our tool, Rust, GPL-3.0)
              ├─ reads AMCE ROM via `ds-rom` (MIT): header, arm9, autoloads, overlay table, BLZ
              ├─ link step: arm-none-eabi-ld (or rust-lld) with generated linker script
              │     MEMORY { NEW : ORIGIN = <arenaLo>, LENGTH = <budget> }  + INCLUDE symbols9.x
              ├─ place: append autoload at arenaLo, bump arenaLo (EFE/Fireflower/NCPatcher method);
              │         overlay-resident hooks patched into overlay N file (or runtime-applied on load)
              ├─ write branches from manifest: ARM B/BL/BLX, Thumb BL/BLX pairs or 8-byte bridges,
              │         "Around" hooks via trampoline (displaced instrs + return), refuse PC-relative displaced instrs
              └─ output: patched files ─► BPS (vs. SHA-1-checked AMCE) + optional .nds for testing
```

**Two delivery phases:**

- **Phase A, prototype in about 1–2 weeks of effort: NCPatcher as the backend.**
  - Generate `ncpatcher.json`, `symbols9.x` and one C shim per hook (`ncp_hook(0x...)` / `ncp_jump` / `ncp_call` calling the Rust `extern "C"` symbol).
  - Link the Rust `staticlib` through `ld_flags`, with `-Wl,--whole-archive` or `KEEP` as needed.
  - Use `nds-extract` / `nds-build` or `ds-rom` around it, and Arm GNU Toolchain GCC for the shims.
  - This proves the ABI, Thumb/ARM bridges and arenaLo placement on melonDS and hardware quickly, using the battle-tested tool.
- **Phase B, own injector (`mkds-inject`), keeping NCPatcher's semantics.**
  - Port the hook kinds (jump/call/hook/over, t-variants, bridge layouts above) to Rust on top of `ds-rom`.
  - This removes the C-shim layer and the GCC requirement (rust-lld can link with a linker script; the target's default linker is lld).
  - It lets the hook manifest come straight from MKDSrust's symbol database (dsd-style configs).
  - Since NCPatcher is GPL-3.0, its bridge code can be ported directly with credit.
  - Optional later step: an RCM-style runtime module loader (Unlicense precedent, MKDS-specific), so several mods can be enabled per boot without re-patching.

**ABI contract for the mod API (shared by both backends):**
- `extern "C"` (AAPCS) functions only.
- `#[repr(C)]` game structs generated from the decomp, with size asserts per platform. The DS is 32-bit, so pointer-size fields on PC must be handled. The flat-memory 32-bit PC build route helps here.
- Fixed-point `Fx32`/`Fx16` newtypes.
- No `std`, no atomics CAS, no floats in hot paths.
- Allocation goes through a `GlobalAlloc` forwarding to the game heap.
- Hooks are identified by **name**. The address, the ARM/Thumb mode and the overlay come from a per-ROM-version table, and only AMCE rev 0 is supported at first.

---

## 7. Risks

1. **Rust Tier 3 / nightly churn.** Build-std or the target spec can break on a given nightly. Mitigation: pin the toolchain in `rust-toolchain.toml` and track the maintainer's issues. Source: https://doc.rust-lang.org/stable/rustc/platform-support/armv5te-none-eabi.html
2. **Code size vs heap.** Rust code with `core::fmt` can be tens of KB. Every byte above arenaLo is lost heap, and nds-bootstrap shrinks the heap further. Mitigation: `opt-level="z"`, LTO, panic=abort, no `fmt`; budget checks in the injector; test under nds-bootstrap with heap shrink enabled.
3. **Overlay code.** Much MKDS race logic is presumably in overlays. Patches must target the overlay files, or be applied at runtime after `FS_LoadOverlay`. Wrong-overlay patches corrupt memory. The manifest must record the overlay ID.
4. **Thumb hook sites.** NCPatcher hooks only from ARM; Thumb jumps take 4–8 bytes and can overrun short functions or split instructions. Mitigation: use the decomp to validate instruction boundaries and PC-relative loads before writing.
5. **Cache coherency for runtime patches.** Runtime writes to code need a DC flush and IC invalidate (RCM added cache invalidation on unload). https://git.randommeaninglesscharacters.com/Garhoogin/RuntimeCodeModules/src/commit/988ef820cebc94559dea40879b51db4dd9551416/source/ldr.c
6. **Distribution cleanliness.** BPS deltas of recompressed binaries may contain rearranged Nintendo bytes. Prefer uncompressed outputs and audit patch contents. Never ship Nintendo SDK headers; NSMB-style templates depend on them.
7. **Compressed-arm9 handling by loaders** (an nds-bootstrap bug class in the past). Test on nds-bootstrap (DSi/3DS SD), at least one popular flashcart kernel, and melonDS/DeSmuME.
8. **Upstream tool status.** Fireflower and Flips are dormant or archived, so only invoke them or replace them. NCPatcher is single-maintainer, so Phase B reduces that dependency.
9. **Region lock.** Addresses are AMCE rev 0 only. XorTroll's work is EU (AMCP), so its addresses differ.

---

## 8. Open items to verify

- MKDS AMCE values: the arenaLo pointer location, the static arm9 size, the free heap during a race (worst-case track and 8 players), and which overlays hold kart/race code.
- Whether RCM supports AMCE, and its loader size.
- NCPatcher: test whether `ld_flags` + `-Wl,--whole-archive` with a Rust `.a` works for code (the hooks still come from C shims).
- rustc `thumbv5te-none-eabi` relocation types emitted with `-C relocation-model=static` vs `pic`, to judge an RCM-style loader.
