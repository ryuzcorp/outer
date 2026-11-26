# Outer brand kit

Source of truth for the Outer mark, lockups, and app icons. SVGs are the
masters; PNGs are checked-in exports so listings and clients can hotlink
them without a build step.

## The mark

Two interlocking rings — an even-odd pair drawn as a single-colour-per-ring
gradient mesh. The geometry is verbatim from the original Penpot export
(`web/public/logo.svg` before this kit); only the paint was cleaned up:
pattern-wrapped `objectBoundingBox` gradients became plain `userSpaceOnUse`
`linearGradient`s. Rendered old-vs-new at 1800 px and diffed: mean per-channel
difference 0.01/255, 99.99% of pixels within 4/255. (The Penpot export had a
1 px pattern-tile seam column; the clean master renders the smooth gradient
there instead — strictly a rasterisation artefact removal.)

Master bounding box (user units): x 65.263–372.815, y 53.880–397.340
(307.552 × 343.460). Ring thickness ≈ 36 units.

## Palette

| Swatch | HEX | RGB | HSL |
|---|---|---|---|
| Violet (stop 0) | `#4634FF` | 70, 52, 255 | 245°, 100%, 60% |
| Sky (stop 1) | `#34A5FF` | 52, 165, 255 | 207°, 100%, 60% |

Gradient axes (screen coords, y down): ring 1 runs `#4634FF` lower-right →
`#34A5FF` upper-left (≈42° above horizontal); ring 2 runs `#34A5FF` bottom →
`#4634FF` top, tilted ≈12° off vertical. The two axes sit 60° apart — that is
the whole "interlock" read; keep both.

Neutrals (from `web/pages/console/console.css`, light / dark):

| Token | Light | Dark |
|---|---|---|
| `base-100` (surface) | `#FFFFFF` | `#19181D` |
| `base-200` | `#F3F2F5` | `#101013` |
| `base-300` | `#E6E5EA` | — |
| `base-content` (ink) | `#141414` | `#F1F0F3` |

App tiles reuse theme surfaces: light tile `#F5F3FF` (violet-tinted white),
dark tile `#101013` (dark `base-200`).

Type: the wordmark is set in **Geist SemiBold** — the site's actual family
(Blume `theme.fonts.display/body: geist`; console `--font-sans: "Geist
Variable"`). Geist is by Vercel, licensed under the **SIL Open Font License
1.1**, which permits outlining. The SVGs carry the wordmark and the OG
tagline as outlines, so no font file is needed to render them.

## Files

| File | Use |
|---|---|
| `mark.svg` | Square (512 viewBox) transparent master; mark fills 80%. Point the MCP `initialize` icons here: `https://outer.now/brand/mark.svg` |
| `mark-circle-safe.svg` | Mark at 76% inside the inscribed circle — **Google** directory listing (square SVG, circular crop, ≥500 px) |
| `app-icon.svg` / `app-icon-rounded.svg` (22% radius) / `app-icon-dark.svg` | Tile icons (light / rounded light / near-black) |
| `mark-black.svg` / `mark-white.svg` | Monochrome, single-colour geometry for one-colour reproduction |
| `lockup-horizontal.svg` / `-dark.svg` | Mark + "Outer" wordmark (dark ink on light / paper on dark) |
| `mark-16/32/48/192/512/1024.png` | Pre-rendered mark rasters (favicons, PWA) |
| `app-icon-512/1024.png` | Pre-rendered tile rasters |
| `og-image.svg` + `/og-image.png` (1200×630, in `web/public/`) | Social card: dark lockup + "Your agent's own app cloud." |
| `openai/logo.png` + `logo-dark.png` (1024) | **OpenAI** plugin `logo` / `logoDark` (`./assets/…`) |
| `openai/icon.png` + `icon-dark.png` (1024) | **OpenAI** plugin `composerIcon` / `composerIconDark` |
| — | **Claude** connector directory: upload `mark.svg` (vector) or `mark-1024.png`; no fixed size is specified, 1024 px satisfies every surface |

OpenAI asset rules (https://developers.openai.com/plugins/deploy/submission):
square, ≥48 px, raster ≤4096 px, ≤5 MiB each — all four files are 1024 px
square PNGs, so they comply. Claude takes one icon in the developer portal;
Google takes the square SVG above.

## Rules

- Clear space: one ring thickness (≈1/8 of mark height) on all sides.
- Minimum sizes: mark 16 px screen / 5 mm print; lockup 72 px / 20 mm wide.
- Don't recolour the rings (monos only where one colour is required),
  don't stretch, rotate, add shadows/outlines, or set the wordmark in
  another face. Don't place the colour mark on saturated backgrounds —
  use `mark-white` on dark photos, `mark-black` for single-colour print.
- Regeneration: geometry lives in `logo.svg`; the kit script that laid out
  these files is not checked in. Export new rasters from the SVGs
  (e.g. `rsvg-convert -w 1024 mark.svg -o mark-1024.png`).
