<p align="center">
  <img src="assets/branding/logo.png" alt="MKDSrust" width="640" />
</p>

# MKDSrust

MKDSrust turns your own dumped **Mario Kart DS (USA, revision 0)** ROM into a native game and a generated Rust workspace. It statically translates the ARM9 and ARM7 programs before play, with native graphics, audio and input. This is a work in progress: Windows is the primary platform, Linux has limited validation, and macOS is untested.

## Purpose

- Preserve playability beyond the original hardware.
- Support modding through generated source and GPU texture replacement.
- Improve performance and presentation while retaining the original driving behavior.
- Make the game's behavior easier to study and learn from.

## Instructions

### Requirements

| Requirement | Details |
|---|---|
| Game | Your clean, self-dumped USA cartridge ROM, code **AMCE**, revision **0** |
| Windows launcher | Windows 10/11 64-bit; the release launcher prepares a private portable GNU Rust toolchain |
| Windows development | Rust 1.96.0; MSVC builds of the launcher require Visual Studio C++ Build Tools |
| Linux | Stable Rust, C/C++ toolchain, pkg-config, ALSA, udev, X11/Wayland and xkbcommon development libraries |
| macOS | Stable native Rust toolchain and Apple command line tools; untested |
| Resources | About 2 GB free disk space; 8 GB RAM recommended; internet for initial dependencies |

For development builds, install Rust from [rustup.rs](https://rustup.rs), then reopen your terminal.
Build on the OS where you intend to play; generated executables are platform specific.

### Windows launcher

1. Open a terminal in this checkout and build the launcher:

   ```powershell
   cargo build --release --locked --manifest-path launcher/Cargo.toml
   .\launcher\target\release\MKDSrust.exe
   ```

2. Follow **Setup**: welcome, ROM verification, portable toolchain installation, local build, then Home.
3. Keep the launcher open during building and playing. Choose **Play** when ready.
4. Use **Settings** and **Controls**, then **Save**; changes apply on the next launch.

The launcher checks ROM compatibility and verifies the bundle before play.
Rebuilding into a nonempty destination stages a sibling folder and switches only after success.

Release downloads need no source checkout, Rust installation, or Visual Studio. The launcher embeds
an allowlisted compressed archive of `1_build_tool`, `2_native_game`, and the address dictionary;
it extracts a versioned copy in its data folder. No ROM, extracted assets, generated workspace,
or compiled game is embedded. Rust 1.96.0 GNU components are downloaded from the official Rust
distribution server and checked against its SHA-256 files. Toolchain downloads support retry
and resume. Initial crate downloads still require internet access. A future release may provide
a separately downloadable archive of vendored lockfile dependencies for offline builds.

**Advanced** offers system Rust, accuracy mode, rebuilding, logs, and folders. **Mods** describes
planned plugin support. Settings and controls remain shared with the builder and game.
`MKDSrust.exe --smoke-test` saves all pages and setup stages to `release_shots/` and exits.
Keep the checkout for future builds. Logs appear in the launcher and are saved beside
`%APPDATA%\MKDS Native Builder\launcher.json` as `launcher-build.log`.

### Command-line build and play

From the repository root on Windows:

```bat
build.bat "C:\path\to\game.nds"
play.bat
```

Double-clicking `build.bat` prompts for the ROM. `mkds.bat` provides a menu for play,
building, controls and display settings.

On Linux or macOS:

```sh
sh build.sh "/path/to/game.nds"
sh play.sh
```

The default output is `builds/mkds/`: the verified `play` launcher, game executable,
extracted assets, generated source and manifest. Windows executables use `.exe`.
Pass a second argument to the build script for a custom empty output folder.
Initial compilation takes time; subsequent builds use cached results.

Normal builds use native, budget-free timing and native FS, ARM7, time and sound services.
For exact instruction timing, rebuild with `--timing exact`; set `MKDS_HW_FS`,
`MKDS_HW_ARM7`, `MKDS_HW_TIME` and `MKDS_HW_SND` to `exact` at runtime.
Launcher **Accuracy mode** sets those runtime switches; exact instruction timing also needs an exact build.

### Controls and settings

| Keyboard | Action |
|---|---|
| Z / X | Accelerate or confirm / brake or back |
| S / A | Hop or drift / use item |
| Arrow keys | Steering and menus |
| C / V | DS X / Y |
| Enter / Right Shift | Start / Select |
| Left mouse on lower screen | Touch input |
| F11 / Esc | Toggle fullscreen / quit |

Controllers support the D-pad or left stick, bottom/right face buttons for A/B,
left/right bumpers for item/drift, and Start/Select. Connect before launch.
Rebind through the launcher or `mkds.bat`, or edit `controls.txt`.

| Platform | Folder for `controls.txt` and `settings.txt` |
|---|---|
| Windows | `%APPDATA%\MKDS Native Builder\` |
| Linux | `~/.config/mkds-native-builder/` |
| macOS | `~/Library/Application Support/MKDS Native Builder/` |

| Setting | Essential options |
|---|---|
| Renderer | GPU (default) or Software |
| Scale | Auto (default, up to 4x) or 1–8; 1 keeps the original resolution |
| Filter | Sharp or Smooth |
| Layout | Stacked or SideBySide |
| Fullscreen | No or Yes |

For one launch, pass options such as `play.bat --scale 1`, `--mute` or `--stats`.
GPU texture replacement: set `DumpTextures` to a folder in `settings.txt`, play to
collect PNGs, then set `TexturePack` to your replacement folder and clear `DumpTextures`.
Keep filenames; replacement dimensions must be an integer multiple from 1–8 of the original.

### Save files

Progress saves automatically under the platform data directory in
`MKDSrust/saves/<original ROM sha1>.sav`. Use **Open save folder** in launcher settings.
A flat 256 KiB `save.sav` beside the executable is imported if no user save exists.
Compatible flat DeSmuME/melonDS saves can be copied to the user save path.
`MKDS_SAVE=off` disables saving; `MKDS_SAVE=<path>` chooses another file.
Record/replay sessions use blank in-memory saves.

## Legal

You must own the game and dump your cartridge yourself. No Nintendo code, assets or
ROMs are included or downloaded; translation and extraction happen locally.
Generated source, extracted assets and built games are for personal use only and must
not be shared, including dumped game textures.

MKDSrust is not affiliated with or endorsed by Nintendo. Nintendo, Mario Kart DS and
all other trademarks belong to their respective owners.

## Credits

- [Wiicompiled](https://github.com/patchzyy/Wiicompiled): static recompilation and platform design inspiration.
- [Wheel Wizard](https://github.com/TeamWheelWizard/WheelWizard): launcher workflow inspiration.
- [GBATEK](https://problemkaputt.de/gbatek.htm), [melonDS](https://github.com/melonDS-emu/melonDS) and [DeSmuME](https://github.com/TASEmulators/desmume): public hardware and architecture references.
- [N64Recomp](https://github.com/N64Recomp/N64Recomp), [ultramodern / N64ModernRuntime](https://github.com/N64Recomp/N64ModernRuntime), [RT64](https://github.com/rt64/rt64) and [aurora](https://github.com/encounter/aurora): translation, native service and presentation references.
- [ds-decomp](https://github.com/AetiasHax/ds-decomp) and public MKDS mapping research: research references; reference status does not grant permission to copy code.
- Rust and dependency authors, including wgpu, eframe/egui and corosensei, and dictionary and verification contributors.

## License

Project code is [GPL-3.0-or-later](LICENSE); this license does not cover Nintendo material or ROM-derived output.

## Documentation

See [MKDSrust-Docs](https://github.com/CameronStorer/MKDSrust-Docs) for technical details,
research, verification procedures and the roadmap.
