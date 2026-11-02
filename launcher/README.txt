MKDSrust for Windows 10/11 x64

Extract this zip and open MKDSrust.exe. Follow Setup to choose your own clean
USA revision 0 cartridge dump (AMCE), install the private portable Rust GNU
toolchain, and build the game locally. No Visual Studio or Rust installation
is required. Internet is needed for official Rust archives, llvm-mingw tools
from GitHub and crates.io on the first build. Allow several GB of free disk
space and time for compilation.
Download failures can be retried; partial toolchain archives are resumed.

The executable contains project source, not a ROM, Nintendo game assets or
translated game code. These are produced only on your computer from your ROM.
Do not distribute generated games, generated source or extracted assets.

Each build goes into its own folder (an MKDSrust subfolder if you pick a folder
with other files). The generated Rust source is in its source folder.

Settings and Controls apply on the next game launch. Advanced contains logs,
build folders, accuracy mode and an optional system Rust setting.

GPL-3.0-or-later. See LICENSE. Independent project, not affiliated with or
endorsed by Nintendo. Nintendo trademarks belong to their respective owner.

Portable storage
----------------
By default keep MKDSrust.exe in a short writable folder, such as
C:\Games\MKDSrust. Everything belongs to this install:
  data\launcher.json, settings.txt, controls.txt, saves\
  data\toolchain\, src\, cache\, modding\
  games\MKDSrust\ (then MKDSrust (2), etc.; includes mods and game logs)
Move or delete the whole folder together. Your original ROM stays where you
selected it. Advanced shows the data location and has Open data folder.

If the executable folder is not writable, the launcher uses the existing
AppData locations. Create MKDSRUST_NOT_PORTABLE beside the executable or in
the data folder (or set that environment variable) to opt out. Restart after
changing this setting. Protocol links are repaired on startup after a move.

On the first portable start, existing AppData data prompts once: Yes copies
settings, controls, saves and launcher preferences; No copies everything,
including the large toolchain/cache; Cancel starts fresh. Originals are
always retained, including when requesting a full transfer. Downloads are
needed again after a small migration. Keep backups until migration is checked.

A game started directly uses MKDS_DATA_DIR when set, otherwise looks for
..\..\data relative to game.exe, then falls back to the old AppData location.
