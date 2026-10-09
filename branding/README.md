# CIWS brand

A sibling of [Warcon's](https://github.com/warcon-app/warcon/tree/main/branding): the same brass
plate with two cut corners, carrying CIWS's own glyph (one line in, a node, three lines out: one
kill feed, sent everywhere). The wordmark is **CIWS** set in Barlow Condensed SemiBold with 6%
tracking, as Warcon's is, traced to outlines so nothing depends on installed fonts.

Everything in this folder is made by `src/generate.ts`. Edit the script, not the files:

```sh
cd branding/src && bun install && bun generate.ts
```

The PNGs need `rsvg-convert` (librsvg).

## Files

| File                                         | Use                                                                                                           |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `ciws-logo-on-dark.svg` / `-on-light.svg`    | Primary horizontal lockup. Pick by background.                                                                |
| `ciws-logo-mono-white.svg` / `-mono-black.svg` | Single-colour lockup for print, embroidery, forced-colour modes.                                            |
| `ciws-stacked-on-dark.svg` / `-on-light.svg` | Square-ish lockup for avatars and tiles.                                                                      |
| `ciws-mark.svg`, `ciws-mark-mono-*.svg`      | Mark alone (favicon, app icon, small spaces under 24px).                                                      |
| `ciws-wordmark-on-dark.svg` / `-on-light.svg` | Wordmark alone.                                                                                              |
| `png/`                                       | Rasterised copies: mark at 16–1024px, lockups at 2x, stacked at 1024px wide, and the 1280×640 social preview. |

## Colours

Warcon's own:

| Token | Hex       | Role                                |
| ----- | --------- | ----------------------------------- |
| Brass | `#D4A843` | Accent, the mark's plate            |
| Ink   | `#0F0F12` | Dark background, glyph on the plate |
| Mist  | `#E8E8E8` | Text on dark                        |

Keep the plate brass whenever colour is available; the mono variants are for one-colour contexts
only. Do not recolour, outline, rotate, add effects, or set the wordmark in another face. Use the
mark alone below 24px in height; the horizontal lockup stays legible down to 20px high.

## Fonts and licence

Barlow Condensed is licensed under the SIL Open Font License (`src/OFL.txt`). The outlines in these
SVGs are derived works you may use freely with the logo. The logo files themselves are covered by
the repository's MIT licence; please do not use them to imply that a service is run by the Warcon
project.
