# Build dictionary

These CSVs contain research metadata used when translating a locally supplied ROM;
they contain no game code or extracted assets.

| File | Contents |
|---|---|
| `FUNCTION_BOUNDARIES.csv` | Function addresses and boundaries for translation |
| `functions.csv` | Function names, signatures, status and notes |
| `sdk_functions.csv`, `game_names.csv` | Refined function metadata applied by address |
| `structs.csv`, `fields.csv` | Structure layouts and field metadata |
| `field_uses.csv` | Evidence linking instructions to fields |
| `globals.csv` | Global address metadata |

Research methods, maintenance scripts and technical notes live in the
[documentation repository](https://github.com/CameronStorer/MKDSrust-Docs).

## Credits
Many function names, and later struct and signature facts, come from the
[tylzars/mkds](https://github.com/tylzars/mkds) matching decompilation (MIT, Tyler Zars and RonyxDumb), used with
the creator's permission. Only names, types and layouts are used; no code from it is included. Rows taken from it say
so in their notes.
