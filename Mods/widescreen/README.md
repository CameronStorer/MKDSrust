# Widescreen

See more of the track: the 3D screen is drawn at 16:10, 16:9 or 21:9 instead of the DS's 4:3. The view extends to
the sides at the same height ("Hor+"), so nothing is stretched or zoomed. The HUD stays in the middle.

## Use it
1. In the MKDSrust launcher, open **Mods**, tick **Widescreen**, then **Rebuild**.
2. In the game, press **F10** → Display → **Widescreen**, and pick 16:10, 16:9 or 21:9.
   Or put `Widescreen = 16:9` in `data/settings.txt`.
3. For a full-width picture on a TV or monitor, also turn on **Single screen** (and F11 for fullscreen).

## How it works
The game's 3D geometry already reaches past the DS screen's left and right edges; the DS simply never showed it. The
GPU renderer keeps those parts and draws them into extra columns on each side (24, 40 or 96 DS pixels per side). The
2D layers (HUD, menus) are drawn in the middle as before; the other screen gets black bars.

## Known limits
- The game hides some objects (karts, items, props) once they leave its own 4:3 view, so they can appear late at
  the far edges.
- 2D screens (menus, the HUD) stay 4:3 in the middle.
- Needs the GPU renderer (the software renderer stays 4:3).

## Files
`mod.json` only: the renderer code ships with MKDSrust (`2_native_game/native/src/gpu`), and this mod switches it on.
