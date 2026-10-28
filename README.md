<p align="center">
  <img src="assets/branding/logo.png" alt="MKDSrust" width="640" />
</p>

# MKDSrust

**Lift the code. Keep the drift.**

MKDSrust turns your own legally dumped **Mario Kart DS (USA, revision 0)** ROM into a native Windows game and a Rust source workspace you can inspect and rebuild. Static recompilation translates the game's ARM9 and ARM7 programs on your computer before play. Linux has limited validation; macOS has an untested build path.

The aim is to preserve playability beyond the original hardware, make the game's behavior easier to study, create room for modding, and improve performance and presentation while retaining the original driving behavior. Generated source has structured control flow, research names and inferred field accessors; readability is still improving rather than being a finished hand-written decompilation.

> [!IMPORTANT]
> You must own the game and dump your cartridge yourself. This repository supplies no Nintendo code, assets or ROMs and does not fetch them for you. Translation and extraction happen locally. Generated source, extracted assets and built games are for personal use and must not be shared.

> [!WARNING]
> This is a work in progress. Verification covers recorded scenarios, not every possible game state. Player builds use native hardware services and budget-free timing by default. Use exact timing and hardware switches for the established acceptance baseline.

MKDSrust is not affiliated with or endorsed by Nintendo. Nintendo, Mario Kart DS and other trademarks belong to their respective owners.

## What you get

- **Native execution:** the normal game build contains compiled translations, without a CPU instruction interpreter or JIT. Windows races have run at real-time speed with clean audio on a 6-core PC; performance depends on your machine.
- **Sharper 3D:** GPU rendering, automatic resolution scaling, fullscreen, filtering and stacked or side-by-side screens. The 2D presentation retains the original pixel resolution.
- **Flexible input:** keyboard and controller rebinding, plus mouse input on the touch screen.
- **Source to explore:** a locally generated Rust workspace with structured functions, comments and inferred field accessors. Some names and types remain tentative.
- **A verified faithful baseline:** a recorded full race matches the project's exact baseline bit for bit in telemetry, inputs and screenshots. The verification scope is explained below.
- **Texture replacement today; more later:** GPU texture dumping/replacement is available. Widescreen camera/HUD changes, HD model hooks and interpolated high-refresh presentation are planned.

## Requirements

- A clean, self-dumped USA ROM: game code **AMCE**, revision **0**. Normal builds reject other regions, revisions and altered dumps.
- **Windows 10/11, 64-bit** for the primary supported experience. Desktop Linux has limited testing; macOS is untested.
- Stable Rust and Visual Studio C++ Build Tools on Windows, as described below.
- About **2 GB free disk space** and **8 GB RAM recommended**. Compilation can consume substantial memory; reduce `--jobs N` if necessary.
- Internet for the initial dependency download; cached dependencies can be used offline later.

MKDSrust keeps existing executable names, output paths and configuration folders for compatibility with existing installs.

## Quick start: graphical Windows launcher

After installing Rust's MSVC toolchain and the C++ build tools, open a terminal at the repository root:

~~~powershell
cargo build --release --locked --manifest-path launcher/Cargo.toml
.\launcher\target\release\mkds_launcher.exe
~~~

On **Home**, select your own supported ROM, inspect its compatibility result, select an output folder and choose **Build game**. Keep the launcher open while building or playing. Logs appear below setup and on **Advanced**. Choose **Play** when ready; the launcher verifies the bundle first.

**Settings** and **Controls** have explicit Save buttons; changes apply on the next game launch. Controller bindings use menus, without physical controller capture. A rebuild into a nonempty destination creates a sibling directory and changes the active output only after success, preserving the old bundle. Keep the source checkout available for future builds.

Preferences and the last completed build log live in `%APPDATA%\MKDS Native Builder\launcher.json` and `launcher-build.log` beside it. **Outdated** compares local source timestamps and the selected ROM hash; it is not an online update check. Advanced offers Accuracy mode (exact hardware), off by default; it sets FS/ARM7/TIME/SND to exact in the game process. See [launcher documentation](launcher/README.md).

## Quick start: Windows scripts

**1. Install Rust** (one time)
- Download and run `rustup-init.exe` from **https://rustup.rs** and accept the defaults.
- If it asks to install the **Visual Studio C++ Build Tools**, say yes (Rust needs them on Windows).
- Close and reopen any terminal windows afterwards.

**2. Get this repository**
- Click **Code -> Download ZIP** on GitHub and unzip it (for example to `C:\MKDSrust`), **or** run
  `git clone https://github.com/CameronStorer/mkds-native-builder`.

**3. Build**
- **Double-click `build.bat`.** A window opens and asks for your game: paste the path of your `.nds` file (Ctrl+V or
  right-click; or drag the file into the window) and press Enter.
  - Tip: in File Explorer, Shift + right-click the `.nds` file and choose **Copy as path**.
- Or from a terminal: `build.bat "C:\path\to\your\game.nds"`.
- The first build takes **about 10-20 minutes** (it downloads the Rust libraries it needs and compiles everything). You will see
  progress in five steps. Rebuilds are much faster (often under a minute).

**4. Play**
- When the build is done, choose **Play** in the same window, or double-click **`play.bat`** any time later
  (or `builds\mkds\play.exe`).

That's it.

**The menu:** double-click **`mkds.bat`** any time to play, rebuild, or change the controls and display settings, all
from one place.

## Quick start (Linux and macOS)

Build on the operating system where you will play; these scripts do not cross-compile.

**1. Install prerequisites**

On Debian/Ubuntu Linux:

```sh
sudo apt-get update
sudo apt-get install build-essential pkg-config curl ca-certificates libasound2-dev libudev-dev \
  libx11-dev libxcursor-dev libxrandr-dev libxi-dev libx11-xcb-dev \
  libwayland-dev libxkbcommon-dev
```

Other Linux distributions need equivalent C compiler, pkg-config, ALSA, X11, Wayland, and xkbcommon development packages.
Playing requires a graphical desktop and a working audio device. WSL2 requires WSLg for the game window;
if ALSA has no default audio device, use `--mute` or install `libasound2-plugins` and configure ALSA to use your
WSLg PulseAudio server. To test audio without changing your system defaults, save this as `alsa-wsl.conf` in
your output folder:

```text
</usr/share/alsa/alsa.conf>
pcm.!default { type pulse }
ctl.!default { type pulse }
```

Then launch from that folder with `ALSA_CONFIG_PATH="$PWD/alsa-wsl.conf" ./play` (WSLg supplies `PULSE_SERVER`).

On macOS, install Apple's command line tools:

```sh
xcode-select --install
```

On either OS, install the current stable Rust toolchain from **https://rustup.rs**, then reopen your terminal.
Use the native Rust toolchain for your processor (including Apple Silicon). macOS support is untested.

**2. Build and play**

Download or clone this repository, open a terminal in it, and run:

```sh
sh build.sh "/path/to/your/game.nds"
sh play.sh
```

In a Git checkout the scripts can also be run as `./build.sh` and `./play.sh`. Scripts work from other directories
too; relative ROM/output paths are relative to your terminal's current directory. Quote paths containing spaces.

For a custom output directory:

```sh
sh build.sh "/path/to/your/game.nds" "/path/to/empty/output"
"/path/to/empty/output/play" --stats
```

`play.sh` starts the default `builds/mkds/play` and forwards all game arguments. Native outputs are OS-specific;
use separate output folders when sharing a checkout between Windows and WSL. The shell builder keeps its own
compiler artifacts under `1_build_tool/target/<OS>-<architecture>`.

## Controls

| Keyboard | DS button | In the game |
|---|---|---|
| **Z** | A | Accelerate / confirm |
| **X** | B | Brake / back |
| **S** | R | Hop / drift |
| **A** | L | Use item |
| **C** / **V** | X / Y | |
| **Arrow keys** | D-pad | Steer / menus |
| **Enter** | Start | Pause / start |
| **Right Shift** | Select | |
| **Left mouse button** (on the lower screen) | Touch screen | |
| **Esc** | | Quit |
| **F11** | | Fullscreen on/off |

**Game controllers** (Xbox, PlayStation, Switch Pro and most others) work too: plug one in before starting the game.

| Controller | DS button |
|---|---|
| **A** (bottom face button) or **right trigger** | A (accelerate) |
| **B** (right face button) or **left trigger** | B (brake) |
| **Y** / **X** (top / left face buttons) | X / Y |
| **Left / right bumper** | L (item) / R (drift) |
| **D-pad** or **left stick** | D-pad |
| **Start** / **Back (View)** | Start / Select |

### Changing the controls

Double-click `mkds.bat` and choose **Change controls**: pick a DS button, press the new key, then **Save and go back**.

Or edit the controls file in any text editor. It is created with the keys above the first time the game starts:

- Windows: `%APPDATA%\MKDS Native Builder\controls.txt`
- macOS: `~/Library/Application Support/MKDS Native Builder/controls.txt`
- Linux: `~/.config/mkds-native-builder/controls.txt`

Each line is `Button = Key`, for example `L = Q`. Give a button several keys with commas (`A = Z, Space`) or none (`Select =`).
Buttons: `A B X Y L R Start Select Up Down Left Right Quit`. Key names: `A`-`Z`, `Key0`-`Key9`, `F1`-`F15`, `Up Down Left
Right`, `Space Enter Escape Tab Backspace Delete Insert Home End PageUp PageDown`, `LeftShift RightShift LeftCtrl RightCtrl
LeftAlt RightAlt LeftSuper RightSuper`, `Apostrophe Backquote Backslash Comma Equal LeftBracket RightBracket Minus Period
Semicolon Slash`, `NumPad0`-`NumPad9 NumPadDot NumPadSlash NumPadAsterisk NumPadMinus NumPadPlus NumPadEnter`,
`CapsLock NumLock ScrollLock Pause Menu`. Changes apply the next time the game starts, and the file survives rebuilds.
If the file has a mistake, the game says which line; delete the file to get the default controls back.
To use a different file once: `play.bat --controls "C:\path\to\controls.txt"`.

Controller buttons have their own lines, starting with `Pad`: for example `Pad L = LB, RT`. Names are by position, so they
work for any controller: `South East North West` (face buttons; South is Xbox A / PlayStation Cross), `LB RB LT RT`,
`Start Select`, `DPadUp DPadDown DPadLeft DPadRight`, `StickUp StickDown StickLeft StickRight` (left stick),
`LeftStick RightStick` (pressed in) and `Guide`. Files without `Pad` lines use the controller layout above.

---

## Display

The 3D graphics and both screens are drawn by your **graphics card** (Direct3D 12 or Vulkan on Windows, Vulkan on Linux,
Metal on macOS). **By default the 3D is drawn as sharp as your window allows** (up to 4x the DS's 256 x 192); menus, map
and HUD keep the DS's own pixels, top screen above the bottom one. The window can be resized and made fullscreen (F11).

**Want the exact original look?** Set `Scale = 1` in `settings.txt`, or start once with `play.bat --scale 1`. A fixed
`Scale = 2` suits 1080p monitors, `3` or `4` 1440p / 4K. The game plays exactly the same at every setting.

Double-click `mkds.bat` and choose **Display settings**, or edit `settings.txt` (in the same folder as `controls.txt`):

| Setting | Values | What it does |
|---|---|---|
| `Renderer` | `GPU` (default), `Software` | `Software` draws everything on the CPU, pixel for pixel like the console (can drop frames on slower PCs) |
| `Scale` | `Auto` (default), `1`-`8` | 3D sharpness as a multiple of 256 x 192. `Auto` picks the sharpest that fits the window, up to 4x; `1` is the original look |
| `Filter` | `Sharp` (default), `Smooth` | How the screens look when stretched to the window |
| `Layout` | `Stacked` (default), `SideBySide` | Top screen above the bottom one, or next to it for wide monitors |
| `Fullscreen` | `No` (default), `Yes` | Start in fullscreen |

Every option is explained inside `settings.txt` itself. For one run, `play.bat --renderer software` or `play.bat --scale 4`
override the file. Graphics cards without compute
shaders (very old ones) automatically fall back to drawing on the CPU.

### Texture packs

The GPU renderer can save every texture the game uses and load replacements, for example sharper hand-made or upscaled
versions:

1. In `settings.txt`, set `DumpTextures = C:\MKDS textures\dump` and play: each texture is saved once as `<id>.png`.
2. Edit or upscale the PNGs you want. A replacement must be the original size times a whole number from 1 to 8 (for example
   a 64 x 32 texture can become 256 x 128). Keep the file names.
3. Put the replacements in a folder and set `TexturePack = C:\MKDS textures\pack` (and `DumpTextures =` to stop dumping).

A replacement with the wrong size is ignored with a message naming the file. Dumped textures come from your copy of the game:
do not share them.

## What the build produces

Everything goes into `builds\mkds\` (ignored by git; manual copying and forced uploads are still possible):

| Item | What it is |
|---|---|
| `play.exe` | Starts the game after checking that none of your game files were changed or are missing |
| `game.exe` | The compiled game |
| `assets\` | The game's files, unpacked from your ROM |
| `source\` | The Rust code generated from your ROM: you can read it and rebuild it yourself |
| `manifest.json` | Fingerprints (SHA-256) of your ROM, the files, the generated code and the program |

On Linux/macOS the executables are named `play` and `game`, with the same bundle layout. Keep their executable
permissions when moving a bundle. A bundle must run on a compatible OS/architecture with the native libraries installed.

**Please do not share `assets\`, `source\` or the built game** - they are made from your copy of the game.

---

## Options

Run `build.bat "game.nds" "C:\some\other\empty\folder"` to build somewhere else. Advanced users can call the builder directly:

```
1_build_tool\target\release\mkds_builder.exe "game.nds" --inspect                 check the ROM only
1_build_tool\target\release\mkds_builder.exe "game.nds" --rip --out FOLDER        only unpack the game files
1_build_tool\target\release\mkds_builder.exe "game.nds" --build --out FOLDER      full build
```

Useful build options: `--offline` (no network, once everything is downloaded), `--jobs N` (how many CPU cores to use),
`--no-cache` (fresh build), `--clear-cache` (delete the build cache), `--no-source` (don't keep `source\`).
The default cache is `%LOCALAPPDATA%\mkds_builder` on Windows, `${XDG_CACHE_HOME:-~/.cache}/mkds_builder` on Linux,
and `~/Library/Caches/mkds_builder` on macOS. Override it with `--cache-dir FOLDER`.
On Unix, call `1_build_tool/target/$(uname -s)-$(uname -m)/release/mkds_builder` for the same advanced options.

Game options (after `play.exe` or `play`): `--mute`, `--stats` (prints speed and frame counters once a second).

---

## Troubleshooting

| Message | What to do |
|---|---|
| `Rust is not installed` / `cargo is not recognized` | Do step 1, then open a **new** terminal window. |
| `Output must be empty` | The folder already has a build. Delete `builds\mkds` or give a different output folder. |
| `Unsupported ROM ... expected AMCE revision 0` | Only the USA version, revision 0, is supported right now. |
| `This ROM differs from the reference` | Your file is not a clean copy (modified or a different dump). |
| `Asset changed: ...` | A game file in `builds\mkds\assets` was edited or deleted. Build again. |
| Linker / `link.exe` errors | Install the Visual Studio C++ Build Tools (step 1) and try again. |
| The build fails with a Cargo error | Run it again with an internet connection; if it still fails, open an issue with the message. |

## How it works

1. **Validate:** identify the ROM and check its SHA-256 against the supported dump.
2. **Extract:** unpack your local game files and record their fingerprints.
3. **Translate and raise:** use function boundaries to translate ARM9 and ARM7 machine code into Rust, then improve control flow, names and field access using the research dictionary. Game instructions come from your ROM, not this checkout.
4. **Compile:** build the translations with the runtime and native window, graphics, audio and input backends. Incomplete generation fails the normal build.
5. **Package and verify:** stage the game, launch verifier, assets, source and manifest. The play executable checks the bundle before launching it.

Static recompilation removes runtime CPU instruction interpretation; the current baseline still uses a DS hardware compatibility model for cycle budgets, timers, DMA, interrupts, memory and the translated ARM7 program. Host graphics and audio provide native presentation, and ten verified leaf functions have native Rust replacements. Native FS, ARM7 PXI services, event-driven sound and OS time/IRQ replacements have merged behind opt-in `MKDS_HW_*` switches. A separate `MKDS_TIMING=native` build removes most ARM9 instruction budgets; the exact baseline remains the default.

The [function dictionary](dictionary/README.md) contains research metadata rather than game code. Only verified native replacements are enabled by default. For developers, `MKDS_NATIVES=all` includes trial replacements and `MKDS_NATIVES=off` selects translations throughout.

Research notes: [MKDS Decomp Docs](https://docs.cameronstorer.com/MKDS-Decomp-Docs/).

## Native defaults and exact builds

Normal builds use budget-free native timing. Player executables default `MKDS_HW_FS`, `MKDS_HW_ARM7`, `MKDS_HW_TIME`, and `MKDS_HW_SND` to `native`; explicit environment overrides are preserved. Native RTC reads the host local date and time. Set `MKDS_RTC=fixed` for the deterministic clock (2010-06-15 12:00:00).

For the original behavior, rebuild with `--timing exact` (or `MKDS_TIMING=exact`), then set all four hardware switches to `exact`. Accuracy mode in the launcher sets those four runtime switches; matching instruction timing requires an exact build. `acceptance.exe` uses its build's generated timing and a fixed RTC: native builds enable native runtime timing and hardware defaults, while exact builds keep exact hardware defaults. Acceptance/reference proofs must build with `--timing exact --with-acceptance`; record/replay paths default to the fixed clock.

## Verification and source progress

The [Block 4 report](4_docs/BLOCK4_SOURCE_PLAN.md) records a Figure-8 acceptance race finishing first at frame **11,462**. Fiber and flat dispatch produced the same **1,707,950,080 ARM9 instructions**, **581,521 IRQs**, byte-identical telemetry over **12,362 frames**, identical inputs and all **40 screenshots**. Raised code and type/accessor passes are also reported race-identical. This proves regression equivalence for the recorded run against the project's exact baseline, rather than universal agreement with real hardware across all modes.

All ARM9 and ARM7 functions have raised T1 output (12,736 functions in the whole-program IR check). The source package includes README.txt and tiers.csv. Flags as locals are implemented (81,372 of 107,472 compares); type coverage reached 26,370 accessor sites, 810 functions with typed arguments and 1,003 with renamed registers (`6b267ef`, `0a4351f`). The latest [name review](NAMES_REPORT.md) changed 109 of 1,532 reviewed uncertain names (87 confirmed, 22 reworded), leaving 1,445 names ending exactly in `_maybe`. The [field review](FIELDS_REPORT.md) renamed 204 of 595 placeholders (58 confident, 146 uncertain), leaving 391. Names, inferred types and accessors are still being refined; names ending in _maybe indicate uncertainty. Exact T0 translations remain available for transfers requiring their dispatch behavior. Shadow checks compare replacement functions against translations.

The [Block 5 census](4_docs/BLOCK5_CENSUS.md) adds recording/replay evidence: exact-vs-exact and flat-vs-fiber runs compare equal. Its route covers boot, menus, one three-lap race and results. There were 19 skipped V-blanks during loading and none in the race loop. Battle, missions and menus outside that route are not yet covered by this census.

## Status

- Linux: builder, recompiler/generator, and native-host tests pass on Debian 13 x86-64 under WSL2. A full ROM build
  and 600-frame WSLg run with audio and scripted input completed successfully, reaching nickname confirmation.
  Recoverable ALSA underruns no longer terminate the game. Full Linux races, physical keyboard/mouse input,
  audio quality, and performance across other Linux desktops have not been validated. WSLg emitted nonfatal
  Wayland decoration/cleanup warnings during the smoke test.
- macOS: native build path and setup instructions provided; not tested on a Mac.
- Windows: menus, Grand Prix races (Mushroom, Flower, Shell cups), Mission 1-1, Battle mode, full races through the results screen;
  real-time speed and clean audio on a 6-core PC. GPU renderer (block 2): checked against the CPU renderer on 549 recorded frames
  (99.99% identical pixels; the differences are single edge pixels) and played at 60 frames per second with sound on an RTX 2060.
  The GPU renderer and controllers have not been tested on Linux or macOS yet.
- Known issues: occasional stutter, the race position badge can show the wrong number, rare visual glitches, the in-game keyboard
  may repeat keys.

### Developer checks

```sh
cargo test --locked --manifest-path 1_build_tool/Cargo.toml
cargo test --locked --manifest-path 2_native_game/Cargo.toml --features gen
cargo test --locked --manifest-path 2_native_game/native/Cargo.toml
sh tests/shell_scripts.sh  # Linux/macOS wrapper checks; no ROM needed
```

Some ROM-dependent tests skip unless their documented environment variables are set; the ignored native-window
test is Windows-only. For a bounded smoke test of an actual Unix bundle, run `./play --frames 300 --stats`
from its output folder (add `--mute` if no audio device is available).

### Roadmap

- **Working:** static translation, native host presentation/input/audio, raised code generation, dictionary-driven names and field access, and ten verified native helpers.
- **In progress (Block 4):** clearer types, names, expressions and source organization. Full source readability remains a goal.
- **Done and merged (Block 5.0-5.3):** recording/replay harness and census; native FS (`MKDS_HW_FS`), ARM7 PXI services (`MKDS_HW_ARM7`), event-driven sound (`MKDS_HW_SND`) and OS time/IRQs (`MKDS_HW_TIME`). Native modes are opt-in; the corpus still needs battle, missions and more menus.
- **Working experiment (Block 5.4):** a budget-free `MKDS_TIMING=native` build finishes 1st and measures about 20% less CPU per frame. Finish work and 5.5 spill/reload removal are in progress. The graphical [launcher](launcher/) provides ROM inspection, building, verified play, settings, controls and experimental FS/ARM7/time toggles; it does not expose every native mode.
- **Optional future presentation:** direct graphics submission, HD model hooks, widescreen projection/HUD adjustments and 120/144 Hz interpolation. Today's side-by-side screen layout does not change the game camera's aspect ratio.

The [Block 5 status](4_docs/BLOCK5_PLAN.md#status-2026-10-28) tracks merged work and remaining conditions. Its [seed-equalized proof](4_docs/masks/native_seed_equalized.md) reports all compared race frames equal in every unmasked region: TIME 10,079, ARM7 10,082, FS 10,077, all three 10,081, and budget-free 10,078. This requires test-only tick pinning, documented masks, re-keyed inputs and loading offsets (4 frames for FS/all three, 18 for budget-free); it does not cover the separate SND switch. Without tick pinning the all-three control diverges and does not finish. The proof build still runs the ARM7 sound CPU and advances cycle-driven hardware in waits; full frame-event scheduling and default promotion remain. WiFi is stubbed; multiplayer support is separate future work.

## FAQ

**Is this an emulator?**

The game CPUs are statically compiled before launch, so the normal bundle does not interpret instructions. A DS hardware model still supplies memory, timing and peripheral behavior. A reference interpreter exists for tests and optional diagnostics.

**Where can I download a ROM or a ready-to-play game?**

Neither is supplied. Dump a cartridge you own and build locally. Do not share ROM-derived bundles or source.

**Why does the first build take so long?**

Your machine translates and compiles the game, and initially downloads and compiles dependencies too. Subsequent builds benefit from caching.

**Can I mod it?**

You can study and rebuild your generated source and use the GPU texture replacement workflow. Direct edits to packaged files can fail manifest verification. Do not distribute original game material or ROM-derived output. More native modding hooks are planned.

**Does higher resolution change physics or unlock the frame rate?**

Resolution settings change presentation while retaining established game behavior. High-refresh interpolation and widescreen camera changes are future work.

**Will other games or regions work?**

The supported pipeline is specific to the clean USA AMCE revision 0 dump. Diagnostic bypasses do not establish compatibility for modified ROMs or other versions.

**How do I report failures?**

Include your OS, error text, the scenario and relevant logs. The graphical launcher saves launcher-build.log. Do not attach a ROM, extracted assets, generated game source or a built game. See Status for known limitations.

## Credits and public references

- [Wiicompiled](https://github.com/patchzyy/Wiicompiled): inspiration for these README topics and static recompilation/platform-layer design. Its supplied README is GPL-3.0 reference material; no sentences, logo or images were reused here.
- [Wheel Wizard](https://github.com/TeamWheelWizard/WheelWizard): broad launcher workflow inspiration, as documented by the launcher; no implementation or assets are included.
- [GBATEK](https://problemkaputt.de/gbatek.htm), [melonDS](https://github.com/melonDS-emu/melonDS) and [DeSmuME](https://github.com/TASEmulators/desmume): public hardware-behavior and architecture references listed in the project plans.
- [N64Recomp](https://github.com/N64Recomp/N64Recomp), [N64ModernRuntime](https://github.com/N64Recomp/N64ModernRuntime), [RT64](https://github.com/rt64/rt64) and [aurora](https://github.com/encounter/aurora): design references for static translation, native services and presentation, not claims of bundled backends.
- [ds-decomp](https://github.com/AetiasHax/ds-decomp) and public MKDS mapping/decompilation research: references discussed with import restrictions in the [source plan](4_docs/BLOCK4_SOURCE_PLAN.md). Reference status does not grant permission to copy code.
- Rust and dependency authors, including wgpu, eframe/egui and corosensei, and contributors to the function dictionary and verification tools.

## License and legal status

**This checkout has no top-level LICENSE file or project license declaration in the reviewed Cargo manifests.** The owner needs to specify redistribution terms for this project's code. Do not infer GPL-3.0 for MKDSrust from Wiicompiled's license. Dependencies and public reference projects retain their own licenses.

The repository contains no Nintendo code, assets or ROMs. You must own the game and make your own lawful cartridge dump. ROMs, extracted files, generated source and compiled game bundles are for personal use and must not be redistributed. Texture dumps are also game-derived material. Access to the tools does not grant rights to distribute the game.

MKDSrust is independent of Nintendo and is neither affiliated with nor endorsed by it. All trademarks remain the property of their respective owners.
## Save files

Players automatically keep progress and options in the platform settings data directory,
under `MKDSrust/saves/<original ROM sha1>.sav`. Use **Open save folder** in launcher settings.
A flat 256 KiB `save.sav` beside `play.exe` is imported when no user save exists;
same-size flat DeSmuME/melonDS images can also be copied into the save folder.
`MKDS_SAVE=off` disables saves; `MKDS_SAVE=<path>` selects a different file.
Acceptance defaults to blank in-memory saves; an explicit path enables persistence for
save testing. Recording/replay always uses blank memory, even with an explicit path.
