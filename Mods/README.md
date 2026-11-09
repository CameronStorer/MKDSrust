# Mods

Mods change what the game does or how it looks, by adding code to the game you build. Pick them in the launcher
(**Mods** page) before you build; they are compiled into your game.

- `Mods/<id>/`: **official** mods, maintained and tested with each MKDSrust release.
- `Mods/community/<id>/`: **community** mods, reviewed by a moderator before they are merged.

Custom tracks, characters and other content are a separate, later project.

## A mod's folder
```
Mods/<id>/
  mod.json     what the mod is (required)
  README.md    how to use it, how it works, known limits (required)
```
`mod.json`:
```json
{
  "id": "widescreen",                  // = the folder name: a-z, 0-9 and -
  "name": "Widescreen",
  "version": "1.0.0",                   // x.y.z
  "authors": ["Your name"],
  "license": "GPL-3.0-or-later",        // GPL-3.0 compatible: MIT, Apache-2.0, BSD, ISC, MPL-2.0, ...
  "category": "Graphics",
  "summary": "One sentence.",
  "description": "A few paragraphs, plain text.",
  "features": ["widescreen"]            // runtime features the mod switches on
}
```
Today a mod switches on runtime features that ship with MKDSrust (the list is `FEATURES` in
`1_build_tool/src/mods_repo.rs`; currently `widescreen`). Mods that bring their own game code (replacing a game
function with an improved one) come next; their code will live in the mod's folder.

After adding or changing a mod, run `mkds_builder --mods-index --write` to update `Mods/index.json` (a test checks
that it is current).

## Rules
1. **Original work only.** No game files, textures, sounds or text from the game, and **never code copied from the
   source generated from your ROM**: that code is derived from the game and cannot be shared. Write your own code
   against the readable names.
2. A GPL-3.0 compatible license.
3. Small and reviewable: a moderator must be able to read every line.
4. Mods compile into people's games: no network access, no reading files outside the game folder, nothing hidden.

## Submitting
Open a pull request that adds `Mods/community/<id>/`. A moderator reviews and tests it; once merged it appears in the
launcher and on mkdsrust.com.
