# Widescreen

See more of the track: the 3D screen is drawn at 16:10, 16:9 or 21:9 instead of the DS's 4:3. The view extends to
the sides at the same height ("Hor+"), so nothing is stretched or zoomed. The HUD stays in the middle.

## Use it
1. In the MKDSrust launcher, open **Mods**, tick **Widescreen**, then **Rebuild**.
2. In the game, press **F10** → Display → **Widescreen**, and pick 16:10, 16:9 or 21:9.
   Or put `Widescreen = 16:9` in `data/settings.txt`.
3. For a full-width picture on a TV or monitor, also turn on **Single screen** (and F11 for fullscreen).
4. Want the race map on that single screen? Turn on **Map in the corner** in F10 (or press **F9**): the touch screen
   (positions and map) appears in the bottom-right corner of the 3D screen. Size and opacity are adjustable
   (`MapOverlay`, `MapOverlaySize`, `MapOverlayOpacity` in settings.txt).
5. Prefer just the track? Choose **Track and heads only** (or press **F9** again): the course outline, the racers'
   heads and the timer float over the 3D view, with the map's background and the positions list left out. While this style shows, the touch
   screen is kept on the game's whole-course map (MKDSrust taps the map for you, as you would); clicking the overlay
   taps the touch screen. F9 cycles Off → Picture → Track.

6. Want the whole game on one widescreen screen, menus included? Choose **Screen layout → Combined** in F10 (or
   `Layout = Combined` in settings.txt). Every menu is rebuilt from both screens: the title bar spans the top, the
   preview (character, kart, course) sits next to the touch screen's buttons over one live backdrop, the title screen
   shows the logo next to its menu, and races fill the screen with the map in the corner (F9: Picture or Track).
   After the finish the race and the results sit side by side. Click the touch-screen part with the mouse as usual;
   clicks elsewhere do not touch. Combined uses 16:9 unless you pick another Widescreen shape.

## Changes
- 1.3.0: Combined layout: one widescreen picture built from both screens, arranged per scene, with transitions.
  Track style keeps the game's whole-course map and leaves out the positions list.
- 1.2.0: Track style for the map overlay (outline and heads only, no background); clicking the overlay taps the
  touch screen.
- 1.1.0: map overlay (F9); no more flickering side bars when both screens show 3D (the pre-race course preview).
- 1.0.0: first release.

## How it works
The game's 3D geometry already reaches past the DS screen's left and right edges; the DS simply never showed it. The
GPU renderer keeps those parts and draws them into extra columns on each side (24, 40 or 96 DS pixels per side). The
2D layers (HUD, menus) are drawn in the middle as before; the other screen gets black bars.

## Known limits
- The game hides some objects (karts, items, props) once they leave its own 4:3 view, so they may appear late at
  the far edges.
- 2D screens (menus, the HUD) stay 4:3 in the middle.
- Needs the GPU renderer (the software renderer stays 4:3; Combined then shows Stacked).
- Combined knows the title screen, the "Select ..." screens, races and results; other screens (setup dialogs, award
  ceremony, multiplayer) show both screens side by side.

## Files
`mod.json` only: the renderer code ships with MKDSrust (`2_native_game/native/src/gpu`), and this mod switches it on.
