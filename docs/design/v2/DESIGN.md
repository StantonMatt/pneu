# Pneu v2: design spec

Status: approved (v2), with the refined launcher icon (v2.1, section 3). Visual reference: `index.html` in this folder (open it in a browser; it is self-contained and works offline). Every phone screen and state is drawn there in light and dark, and every PC surface in Plasma light and dark. Icons: `assets/`.

v2 is a visual and UX redesign of v1 (`../DESIGN.md`). It keeps every feature, setting, default, flow and behaviour rule from v1 and from the shipped app (including additions made during implementation, such as Local network, Receive clipboard from PC, Show in share sheet, Quick Settings tiles, the pairing confirmation and the local-queue banner). Where this document is silent, v1 and the code win.

Conventions from v1 still apply: `{pc}` is the PC's name from the pairing QR, `{phone}` is the phone's device name, `…` is U+2026, `·` is U+00B7 with a space each side. Phone strings use sentence case; PC menu items, buttons and window titles use Title Case (KDE HIG).

---

## 1. Direction

Pneu should feel like a small, finished product: calm, exact and quick to read. The redesign does that with four rules.

1. **Indigo means live.** One fixed brand colour, `#4F5BFF`, marks the connection and the primary action. Amber means paused or needs attention, red means broken, grey means offline. Nothing else is coloured.
2. **The Link.** Home (and the PC window) lead with the connection itself: phone tile, tube, PC tile. The tube's style carries the state; a capsule runs along it while something is sending. This replaces v1's status disc and is the one custom component.
3. **Native, not stock.** System fonts, One UI proportions, edge-to-edge, predictive back, true-black dark mode on the phone; Plasma's own light/dark scheme on the PC. Pneu's own tokens replace dynamic (wallpaper) colour.
4. **Quiet by default.** No subtitles, taglines or helper text. A second line appears only where this spec lists it, because it carries a reason, a path, a count or a time.

---

## 2. What changes versus v1

| Area | v1 | v2 |
|---|---|---|
| Colour | Material 3 dynamic colour from the wallpaper | Fixed Pneu palette (section 4.1). Semantic status colours. |
| Icon | Teal carrier with brass caps | Monogram (section 3): a lit tube P with an amber carrier at large sizes, one flat P at small sizes. |
| Home hero | Status disc, title, detail, buttons | The Link, title, detail, buttons. Same states, copy and buttons. |
| Button emphasis | Left tonal, right outlined | Send clipboard / Open Tailscale tonal; Pause neutral; Resume and Pair again filled (the only filled buttons on Home). |
| Banner | Red container, one line | Neutral card with an amber icon tile, up to two lines, action on the right. Local-queue banner keeps red. |
| Recent progress | Ring around the Cancel button | 3 dp bar under the row text; Cancel stays as the trailing button. |
| Queue menu | Plain items | Don't send any is red (it acts without confirmation). |
| Settings order | Send automatically, Connection, Clipboard, Paired PC, This phone | **Paired PC card on top**, then Send automatically, Clipboard, Connection, This phone. |
| Section headers | `titleSmall` in primary colour | 13 sp semibold in secondary grey. |
| Switch | M3 switch with check in the thumb | Pneu switch, no thumb icon (section 5.6). |
| Setup | Large title + card | App icon above the title on first run; rows get 36 dp icon tiles; done rows show an indigo check. |
| Pair scanner | Full outline square, visible title "Pair with PC" | Corner brackets; title kept only as the TalkBack pane title; success fills the frame. |
| Dialogs | Centred M3 AlertDialog | Bottom-anchored floating dialog (One UI placement), two equal-width buttons, red destructive confirm. Same titles, text and buttons. |
| PC window | Native Breeze widgets, Close button, phone icon + status dot | Pneu components following Plasma light/dark; the Link on top; Save to and Unpair… in one card; **no Close button** (title-bar ✕ and Esc remain). |
| Tray | Carrier glyph | New glyph; disconnected = glyph at 50 % opacity; receiving = indigo glyph; paused/error badges in Plasma's neutral/negative colours. |

**Behaviour and navigation.** No functional change. Navigation graph, launch routing, back behaviour, all toggles and their defaults, queue/pause/clipboard rules, notifications, tiles and share target behave exactly as today. The changes that touch interaction are placement only: the Paired PC group moves to the top of Settings, the PC window loses its Close button (closing behaviour is unchanged: it hides), and Unpair… moves from the window footer into the settings card.

**Copy.** Unchanged, except that the Pair screen no longer draws "Pair with PC" (it remains the pane title for accessibility). Copy added during implementation is now specified here (section 7).

---

## 3. Icon

### 3.1 Concepts

| | File | Idea | Strengths | Weaknesses |
|---|---|---|---|---|
| **A · Monogram (chosen, refined)** | `concept-a-monogram.svg`, `concept-a-monogram-glyph.svg` | The P is one bent tube. At launcher size it is drawn lit, with an amber carrier tumbling out of the stem's open end. | Unmistakably Pneu at launcher size (tube, carrier, arrival). Small sizes keep the same P as one flat path. | The carrier shrinks to an amber speck below 24 px (kept; it still reads as a P). |
| B · Transit | `concept-b-transit.svg`, `concept-b-transit-glyph.svg` | A carrier mid-flight inside a glass tube, amber nose first. | Tells the pneumatic-tube story literally. | At 16–24 px the walls thin to hairlines and it reads as a slashed circle; the capsule alone reads as a pill. |
| C · Trail | `concept-c-trail.svg`, `concept-c-trail-glyph.svg` | Three discs accelerating up and to the right. | Very simple, scales cleanly. | Abstract; no link to tubes or the name. |

B and C are unchanged from the first round and kept for reference only.

**Why the carrier leaves the stem, not the bowl.** The P's bowl ends pointing back at the stem, so anything leaving it would hit the stem (tested: it reads as a stick across the letter). The stem's bottom is the tube's other open end, and it points into the empty space under the letter. Real tube stations deliver the same way: the carrier drops out of the bottom of the tube. Tilting the carrier 28° keeps it from reading as the dot of an exclamation mark.

### 3.2 Mark: two masters

Launchers show only the central 72 dp of the 108 dp layers; all sizes below refer to that visible area. Previews in `index.html` are cropped to it.

**Large master** (launcher, PC app icon, in-app mark; 28 dp and up). Coordinates in the 108 dp canvas:

| Layer | Spec |
|---|---|
| Background | Linear gradient `x1 32.4, y1 0 → x2 75.6, y2 108`: `#6B74FF` 0, `#4846E8` 0.55, `#2620AE` 1. Plus a radial top light: centre (54, −8.64), radius 84.24, `#FFFFFF` at 32 % → 0 %. |
| Tube centreline | `M43.95 59.59 V43.09 A8.58 8.58 0 0 1 52.52 34.51 H55.83 A10.23 10.23 0 0 1 55.83 54.97 H53.69`; stroke 8.58, round caps and joins. The stem is shortened by about a quarter of the letter height to make room for the carrier. |
| Tube colour | Linear gradient from (35.37, 25.93) `#FFFFFF` to (66.06, 68.17) `#D9DDFF`: white at the lit top-left, a cool lavender at the bottom-right. |
| Soft shadow | Three copies of the tube and carrier in `#100A6B`, offset down 0.85 / 2.03 / 3.38 dp, widened by 1.0 / 2.6 / 4.4 dp, at 10 / 7 / 5 % opacity. Lives in the foreground layer so it moves with the mark on parallax. |
| Specular streak | The centreline offset by (−1.63, −1.63), from 62 % up the stem, over the bend and top bar, round the bowl to −15°; stroke 1.2, round caps; vertical gradient `#FFFFFF` 0 % at y 52.99 → 95 % at y 43.09 so it fades towards the open end. |
| Carrier | Stadium, 12.87 × 6.86 dp, axis 62° from horizontal (tilted 28° off vertical, falling to the right), its upper end 2.24 dp below the stem's cap. Fill: linear gradient `#FFD98C` → `#FF9426` (top-left to bottom-right). Highlight: a 4.6 × 1.4 stadium on its upper-left side, white at 70 %. |
| Extents | Farthest painted point 26.2 dp from the centre (safe zone 33). Bounding box x 39.7–70.3, y 30.2–78.6 (shadow excluded): the mark is about two thirds of the visible height. |

**Small master** (status bar, notifications, both tiles, tray, themed icon; flat, one path, no carrier):

```
M40.6 75.2 V43.2 A10.4 10.4 0 0 1 51 32.8 H55 A12.4 12.4 0 0 1 55 57.6 H52.6     (108 canvas, stroke 10.4)
```

Round caps and joins, no fill. It is the same P with the stem at full length (the carrier's place). Stroke = 0.197 × letter height; heavier relative strokes at small grids: 3.4 of 24 (`ic_stat_pneu`), 3.1 of 22 (tray). The monochrome (themed) layer uses this path at 0.72 scale (stroke 9.36), centred, so it has the launcher mark's visual weight.

### 3.3 Asset set

All files are plain SVG with baked coordinates: no transforms, masks, clip paths, filters or raster. Strokes, linear and radial gradients and per-stop opacity are used deliberately; Android VectorDrawable (via `aapt:attr` gradients), QtSvg and resvg all support them.

| File | Canvas | Use |
|---|---|---|
| `ic_launcher_background.svg` | 108 | Adaptive background: indigo gradient + radial top light (two paths). |
| `ic_launcher_foreground.svg` | 108 | Adaptive foreground: shadow, lit tube, streak, carrier (10 paths). |
| `ic_launcher_monochrome.svg` | 108 | Themed icon (`<monochrome>`): flat small-master P. Alpha only. |
| `ic_launcher_preview.svg` | 108 | Both layers flattened, for docs and mocks (crop to `18 18 72 72` to see what a launcher shows). |
| `ic_stat_pneu.svg` | 24 | Notification small icon (status, alerts, share admission) and both Quick Settings tiles. Flat small master, white, alpha only. |
| `pneu.svg` | 128 | PC app icon: `.desktop` `Icon=`, window icon, notification `app_icon`, tooltip. Rounded square (radius 30 of 112, 8 px margin) with the launcher background, a 1.5 px inner top-edge light (white 38 % → 0), and the large master scaled to the square. Install to `~/.local/share/icons/hicolor/scalable/apps/pneu.svg` and replace `desktop/assets/pneu.svg`. One file serves 16–256 px; at 16–22 px the carrier is an amber speck and the P still reads. |
| `pneu-mark.svg` | 24 | In-app mark (Home top bar, 28 dp): launcher background in a rounded square, flat white tube and flat `#FFB547` carrier (no shadow or streak). |
| `pneu-tray-connected.svg` | 22 | Small master in `ColorScheme-Text`. |
| `pneu-tray-disconnected.svg` | 22 | Same at `stroke-opacity: 0.5`. Used for no phone paired, pairing in progress, phone not connected. |
| `pneu-tray-receiving.svg` | 22 | Small master in fixed `#6370FF` (≥ 3.5 : 1 on Breeze light and dark panels). Shown at least 1.5 s. |
| `pneu-tray-paused.svg` | 22 | Small master + `ColorScheme-NeutralText` badge (disc r 4.5 at 17.3, 17.3 with two knocked-out bars). |
| `pneu-tray-error.svg` | 22 | Small master + `ColorScheme-NegativeText` badge with a knocked-out "!". |

The tray glyph sits slightly left of centre so badges occupy the empty lower-right corner without overlapping the bowl; no knockout ring is needed. Tray files keep v1's `current-color-scheme` stylesheet, so both the named-icon path and the ARGB fallback (resvg with `currentColor` replaced by the panel text colour) work unchanged.

---

## 4. Tokens

### 4.1 Phone colour

Fixed palette; dark mode follows the system setting. No dynamic colour.

| Token | Light | Dark | Use |
|---|---|---|---|
| `bg` | `#F2F3F6` | `#000000` | Screen background |
| `surface` | `#FFFFFF` | `#141519` | Cards, Link card, Recent card, banner |
| `raised` | `#FFFFFF` | `#1C1D22` | Dialogs, menus |
| `fill` | `#EEF0F4` | `#22242A` | Neutral buttons, icon tiles, thumbnails' placeholder |
| `fill2` | `#E3E6EC` | `#2C2F36` | Progress track, idle tube, pressed rows |
| `separator` | `#E8EAEF` | `#23252B` | Hairlines inside cards |
| `hairline` | none | `#FFFFFF` at 6 % | 1 px inner border of cards and dialogs in dark only |
| `text` | `#0D0F14` | `#F3F4F6` | Primary text and icons in buttons |
| `text2` | `#5C6270` | `#A0A5B1` | Secondary lines, row icons, section headers |
| `text3` | `#8B91A0` | `#7A7F8C` | Chevrons, empty-state icon, version, dashed tube |
| `accent` | `#4F5BFF` | `#4F5BFF` | Live connection, filled buttons, switch on, progress, checks |
| `onAccent` | `#FFFFFF` | `#FFFFFF` | Text on accent (4.9 : 1) |
| `accentText` | `#3D47E0` | `#9AA3FF` | Text buttons, "{n} waiting", sending line |
| `accentSoft` | `#ECEEFF` | `#1D2046` | Tonal buttons, live endpoint tiles, PC tile in Settings |
| `onAccentSoft` | `#2F38C8` | `#C7CCFF` | Content on `accentSoft` |
| `warn` | `#B45309` | `#FFB547` | Paused, banner icon, "{n} need attention" |
| `warnSoft` | `#FFF1DC` | `#2D2210` | Paused endpoint tile, banner tile |
| `danger` | `#D63B3B` | `#FF7A7A` | Errors, failed rows, Unpair row, Don't send any |
| `dangerFill` | `#D93D42` | `#D93D42` | Destructive dialog button (white text) |
| `dangerSoft` | `#FDECEC` | `#331618` | Pairing-lost tile and badge, local-queue banner tile |
| `spark` | `#FFB547` | `#FFB547` | Capsule nose. Only for something in transit. |
| `switchOff` | `#D9DCE3` | `#3A3D45` | Switch track off |
| `inverse` / `onInverse` | `#1C1D22` / `#F3F4F6` | `#F3F4F6` / `#0D0F14` | Snackbar |
| `scrim` | `#0D0F14` at 38 % | `#000000` at 62 % | Behind dialogs |
| Link gradient | `#7381FF` → `accent` | same | Connected tube (left to right) |

Contrast: all text pairs meet WCAG AA (4.5 : 1) except `text3`, which is only used for non-text affordances and the version caption.

### 4.2 PC colour (Slint window)

Chosen by Plasma's scheme (`Palette.color-scheme`). The window background is always `Palette.background`, so custom colour schemes still blend with the Breeze title bar.

| Token | Light | Dark |
|---|---|---|
| `window` | `Palette.background` (Breeze `#EFF0F1`) | `Palette.background` (Breeze `#202326`) |
| `card` | `#FFFFFF` | `#2A2E32` |
| `cardBorder` | `#DCDEE1` | `#373B41` |
| `text` | `Palette.foreground` | `Palette.foreground` |
| `text2` | `#5E6369` | `#A4A9B0` |
| `accent` | `#4F5BFF` | `#4F5BFF` |
| `accentSoft` / `onAccentSoft` | `#E8EAFF` / `#2F38C8` | `#2B305E` / `#C7CCFF` |
| `button` / `buttonBorder` | `#F2F3F5` / `#C8CBD0` | `#33373C` / `#4A4F56` |
| `danger` (text) | `#C9302F` | `#FF7A7A` |
| `dangerFill` | `#D93D42` | `#D93D42` |
| `dangerSoft` / banner border | `#FBE9EA` / `#DA4453` at 35 % | `#3A1F22` / same |
| `warn` / `warnSoft` | `#B45309` / `#FFF1DC` | `#FFB547` / `#3A2C14` |
| `dim` (behind the unpair popup) | `#14161A` at 32 % | `#000000` at 45 % |
| QR card | `#FFFFFF`, always | `#FFFFFF`, always |

### 4.3 Type

System font on both platforms: One UI Sans (or whatever the user picked) on the phone via `FontFamily.Default`; Noto Sans (Plasma default) on the PC. No bundled fonts. Phone sizes in sp; letter spacing in sp.

| Style | Size / line | Weight | Tracking | Used for |
|---|---|---|---|---|
| `largeTitle` | 32 / 38 | 600 | −0.6 | Settings, Set up Pneu, Permissions and battery (expanded bar) |
| `title1` | 26 / 32 | 600 | −0.5 | Link card status |
| `title2` | 20 / 26 | 600 | −0.3 | Home wordmark, collapsed bars, dialog titles |
| `headline` | 17 / 22 | 600 | 0 | PC name in the Settings card, Pair instruction |
| `body` | 16 / 22 | 400 | 0 | Row titles |
| `bodyStrong` | 15 / 20 | 500 | 0 | Banner text |
| `button` | 15 / 20 | 600 | 0 | Buttons (small buttons 14 / 18) |
| `subhead` | 14 / 19 | 400 | 0 | Secondary lines, Link detail, dialog text (15 / 21) |
| `section` | 13 / 18 | 600 | 0 | Section headers, Recent header, "{n} waiting" |
| `caption` | 12 / 16 | 400 | 0 | Version |

PC: base is the system default size (Noto Sans 10 pt ≈ 13.3 px). Phone name 15 px bold; popup title 15 px bold; labels ("Save to") 12.5 px in `text2`; everything else base size.

### 4.4 Space, shape, elevation

| Token | Value |
|---|---|
| Grid | 4 dp |
| Screen gutter | 16 dp (content max width 600 dp, centred) |
| Card padding | rows 16 horizontal; Link card 24 top, 20 sides, 20 bottom |
| Section header | 22 top, 8 bottom, 16 start (aligned with row text) |
| Rows | min 56 dp (one line), 68 dp (two lines), 60 dp permission rows; Recent rows 68 dp, 12 vertical padding |
| Separators | 1 px `separator`, inset 16 dp from the start; 66 dp after a 36 dp tile; 76 dp after a 44 dp thumbnail; none above the first row |
| Radius | 10 thumbnails and small buttons · 11 icon tiles (36 dp) · 14 buttons, 44 dp tiles, snackbar · 16 large button, menu · 17 Link endpoints (52 dp) · 20 banner · 24 cards · 28 dialogs |
| Elevation 0 | cards: flat on `bg`; dark adds the 1 px `hairline` inner border |
| Elevation 1 | menus, snackbar: light `0 4 16 #0D0F14/10 %` + `0 1 2 /6 %`; dark `0 8 24 #000/55 %` + hairline |
| Elevation 2 | dialogs: light `0 18 48 /20 %` + `0 2 6 /8 %`; dark `0 18 48 #000/70 %` + hairline; plus `scrim` |

PC: window padding 16, gap between blocks 12; card radius 14 with 1 px `cardBorder`; rows 12 × 14 padding; buttons 30 px tall, radius 8, 14 px side padding; Link tiles 40 px radius 12, tube 3 px; popup radius 14, padding 20; banner radius 10, padding 10 × 12.

### 4.5 Motion

| Token | Value | Use |
|---|---|---|
| `ease` | cubic-bezier(0.2, 0, 0, 1) | Default for everything |
| `easeOut` | cubic-bezier(0.05, 0.7, 0.1, 1) | Enter (dialogs, menus, snackbar) |
| `easeIn` | cubic-bezier(0.3, 0, 0.8, 0.15) | Exit |
| `fast` | 120 ms | Press feedback |
| `short` | 200 ms | Switch thumb, colour changes, status text cross-fade |
| `medium` | 280 ms | Dialog enter (slide up 24 dp + fade), Link endpoint/tube colour |
| `long` | 400 ms | Pair success fill |
| Press | scale 0.97 on buttons, tiles and the Link card's buttons; pressed overlay `text` at 6 % on rows; no ripple |
| Capsule | 1.4 s loop, `cubic-bezier(.45,0,.2,1)`: fade in at the phone end, travel to the PC end, fade out. Runs only while a transfer is active. |
| Connecting | tube dashes march left to right, 0.8 s per dash period, linear |
| Recent list | `animateItem()` (placement spring, fade 200 ms) |
| Reduced motion | When the system animator scale is 0 or "Remove animations" is on: no capsule run (it rests in the middle of the tube), no marching dashes, cross-fades only. |

### 4.6 Iconography

Material Symbols **Rounded**, weight 400, grade 0, optical size 24, imported as vector drawables (as v1). Size 24 in bars and Link tiles, 20 in buttons, banner tiles and permission tiles, 22 in Recent placeholders and trailing actions. New symbols needed: `smartphone`, `link_off`, `chevron_right`, `expand_more` (replaces `arrow_drop_down`), `lan` (Local network row). The PC window uses the same symbols as SVG, colourised by Slint.

---

## 5. Phone components

### 5.1 Top bars

- **Home bar**: 64 dp. Start 20 dp: `pneu-mark` 28 dp, gap 12, "Pneu" in `title2`. End: `settings` icon button (48 dp, icon 24 in `text2`, description **Settings**). Background `bg`; when content scrolls under it, a 1 px `separator` appears at its bottom edge.
- **Large title bar** (Settings, Permissions and battery, Set up Pneu): back row 56 dp with `arrow_back` (48 dp button, description **Back**), then the title in `largeTitle`, start 20, 6 top, 14 bottom. Collapses on scroll into the 56 dp row with the title in `title2`. Set up Pneu has no back button and adds the app icon (56 dp, `ic_launcher_preview` in a 17 dp-radius squircle) 28 dp from the top, title 16 dp below it.

### 5.2 Card, section header, rows

- Card: `surface`, radius 24, clips its rows.
- Section header: `section` style in `text2`, sentence case.
- Toggle row: title `body` (+ optional second line `subhead`), trailing Pneu switch; the whole row toggles. Disabled: the text column and switch at 40 % opacity, not clickable.
- Navigation row: title + optional second line; trailing `chevron_right` 20 dp in `text3`.
- Action row: a single `body` line in `danger` (Unpair).
- Permission row: leading 36 dp tile (radius 11, `fill`, icon 20 in `text2`), title, optional second line, trailing small tonal button or a 24 dp filled `check_circle` in `accent`.

### 5.3 Buttons

| Kind | Fill | Content | Height / radius | Use |
|---|---|---|---|---|
| Primary | `accent` | `onAccent` | 48 / 14 (large: 52 / 16) | Resume, Pair again, Continue, Pair, Replace, Try again, Allow camera, Open settings |
| Tonal | `accentSoft` | `onAccentSoft` | 48 / 14 | Send clipboard, Open Tailscale |
| Neutral | `fill` | `text` | 48 / 14 | Pause, Cancel, Close |
| Destructive | `dangerFill` | white | 48 / 14 | Unpair (dialog) |
| Small tonal | `accentSoft` | `onAccentSoft` | 34 / 10, 14 sp | Allow, Allow all, Open, Settings, Dismiss in permission rows |
| Text | none | `accentText` | 48 / 14, 10 dp padding | Fix, Don't send any (banner), Try again (load failure) |

Icons 20 dp, 8 dp before the label. Disabled = 40 % opacity and not focusable. Paired buttons share the row equally with a 10 dp gap; at font scale > 1.3 they stack.

### 5.4 The Link

Row 56 dp tall, 6 dp side inset inside the Link card. Endpoints: 52 dp tiles, radius 17, icon 24 (`smartphone` left, `computer` right). Tube: 4 dp tall, radius 2, 12 dp from each tile. Centre badge: 32 dp circle with a 4 dp ring in `surface` that cuts the tube around it; icon 18.

| State | Phone tile | PC tile | Tube | Badge |
|---|---|---|---|---|
| Connected | accentSoft / onAccentSoft | accentSoft / onAccentSoft | solid, Link gradient | none |
| Sending | as connected | as connected | `accentSoft`, with the capsule running | none |
| Connecting… | fill / text2 | fill / text2 | dashed `text3` (6 on, 7 off), marching | none |
| PC unreachable (incl. Tailscale off) | accentSoft | fill | dashed `fill2`, static | `cloud_off`, `text2`, 1.5 dp `fill2` ring |
| Waiting for Wi-Fi | accentSoft | fill | dashed `fill2`, static | `wifi_off` |
| No network | fill | fill | dashed `fill2`, static | `cloud_off` |
| Paused | warnSoft / warn | fill | solid `fill2` | `pause` on warnSoft / warn |
| Pairing lost | fill | dangerSoft / danger | `dangerSoft` with a gap in the middle third | `link_off` on dangerSoft / danger |

Capsule: 24 × 12 dp, radius 6, `accent` body, 6 dp `spark` nose on the leading (PC) side separated by a 1.5 dp `surface` slit, 3 dp `surface` ring so it reads over the tube. The Link is decorative for TalkBack; the status title and detail carry the meaning.

### 5.5 Link card

`surface`, radius 24 (Home's only large card). Order: Link, 16 dp, status in `title1`, 4 dp, detail in `subhead` `text2` (max 2 lines, centred), 20 dp, button row. Status text cross-fades (200 ms) on change; tiles and tube animate colour (280 ms).

### 5.6 Switch

52 × 32 dp track, radius 16; 26 dp white thumb (inset 3 dp) with a soft shadow (`0 1 2 #000/18 %`, `0 2 6 #000/10 %`). On: track `accent`, thumb at end. Off: track `switchOff`. Thumb slides 200 ms `ease`. Disabled: 40 % opacity. No icon in the thumb. Minimum touch target is the whole row.

### 5.7 Banner

Card `surface`, radius 20, padding 10 / 4 / 10 / 12, margin below 12. Leading 32 dp tile radius 10 (`warnSoft` + `warning` 20 in `warn`; the local-queue banner uses `dangerSoft` + `error` in `danger`). Text `bodyStrong`, up to 2 lines then ellipsis. Trailing text button. The whole banner is one touch target for its action.

### 5.8 Recent

Header row: "Recent" (`section`, `text2`) at start 16; when items wait, a text button "{n} waiting" + `expand_more` 18 in `accentText` at the end, opening the menu (5.9). Card of rows: leading 44 dp thumbnail (radius 10; images centre-cropped; videos get an 18 dp black-55 % disc with a 12 dp white `play_arrow` at bottom-start, inset 4) or a 44 dp `fill` tile with a 22 dp icon (`description`, `link`, `content_paste`, `image`). Title `body`, one line (file names middle-ellipsis). Second line `subhead`, one line. Trailing 40 dp icon buttons (22 dp icons, `text2`). Sending rows add a 3 dp progress bar (`accent` on `fill2`, radius 2) 7 dp under the second line, as wide as the text column. Empty: card 132 dp tall with `image` 28 in `text3` and "Nothing sent yet" in `subhead` `text2`.

### 5.9 Menu, dialog, snackbar

- **Menu**: `raised`, radius 16, padding 6, elevation 1, min width 196. Items 44 dp, `body`-sized 15 sp regular, padding 14, pressed fill `fill` radius 10. Destructive items in `danger`. Anchored under its button, end-aligned.
- **Dialog** (replaces AlertDialog everywhere): floating card anchored to the bottom: 12 dp from the sides, 12 dp above the navigation inset; `raised`, radius 28, padding 24 / 20 / 20, elevation 2 over `scrim`. Optional header: a 44 dp tile (radius 14, `accentSoft`, `computer` 24 dp) 16 dp above the title, used only by the pairing dialogs. Title `title2`, start-aligned, wraps. Optional text `subhead` 15 / 21 in `text2`, 8 dp below. Buttons 24 dp below: two equal-width 48 dp buttons, dismissive (neutral) at start, confirming (primary or destructive) at end; a dialog with only a dismissive action shows one full-width neutral button. Back and tapping the scrim dismiss unless the dialog says otherwise. Enter: slide up 24 dp + fade, 280 ms `easeOut`; exit 160 ms `easeIn`.
- **Snackbar**: `inverse` / `onInverse`, radius 14, padding 14 × 16, 12 dp from the sides, 16 dp above the navigation inset, elevation 1, `bodyStrong` regular weight.

---

## 6. Phone screens and states

Navigation is unchanged from v1 section 3.2 (Setup → Pair → Home; Home → Settings → Permissions and battery; banners → Permissions and battery; Pair again → Pair; Unpair → Pair with the back stack cleared). Predictive back throughout.

### 6.1 Launch

| State | What shows |
|---|---|
| Loading (settings or identity not ready) | `bg`, a 36 dp indeterminate spinner (3 dp, `accent` on `accentSoft`), centred. |
| Load failed (after 5 s) | Centred: **Couldn't load Pneu settings** (`body`), text button **Try again** below. |

### 6.2 Setup (first run) and Permissions and battery

Same composable, two modes. First run: app icon, large title **Set up Pneu**, no back; a **Continue** primary large button pinned at the bottom (full width minus 16 dp margins, above the navigation inset, on a 24 dp `bg` fade). From Settings or a banner: back + large title **Permissions and battery**, no Continue, no icon.

One card. Rows in this order (the rules for detecting, requesting and re-checking each item are unchanged from v1 3.3 and the shipped code):

| Row | Icon | Not done: button | Not done: second line | Shown when |
|---|---|---|---|---|
| **Local network** | `lan` | **Allow**, or **Settings** once denied for good | none | Android 17 (API 37) and later |
| **Notifications** | `notifications` | **Allow**; **Settings** when denied for good or the Status channel is blocked | **Turned off in settings** when the button is Settings | always |
| **Photos and videos** | `photo_library` | **Allow**; **Allow all** when limited; **Settings** when denied for good | **Limited access** / **Turned off in settings** | always |
| **Unrestricted battery use** | `battery_full` | **Allow** | none | always |
| **Pneu was stopped in the background** | `bedtime_off` | **Dismiss** | none | background-kill evidence exists and (Never sleeping apps is satisfied, or the phone isn't Samsung) |
| **Never sleeping apps** | `bedtime_off` | **Open** | **Add Pneu to the list** | Samsung only |

Done rows show the indigo `check_circle` (content description **Granted**). Continue is always enabled.

### 6.3 Pair

Always dark, in both themes (status and navigation bars light-on-dark). Full-bleed camera preview.

- Scrim: black 60 % everywhere except a 264 dp rounded square (radius 36), centred horizontally, its top 200 dp below the status bar.
- Corner brackets: four L-shapes, 4 dp white strokes, 46 dp arms, following the square's 36 dp corners.
- Back: 44 dp glass button (white 14 % + blur) at 12 dp start, 8 dp below the status bar, white `arrow_back`, description **Back**.
- Instruction: 28 dp below the square, centred, white `headline`: **Scan the QR code on your PC**.
- Accessibility pane title: **Pair with PC** (not drawn).

| State | What shows |
|---|---|
| Scanning | As above. |
| Confirm pairing | Dialog over the scanner (dark): PC header tile, title **Pair with {pc}?**, buttons **Cancel** · **Pair**. On Android 17+ without local-network access the confirm reads **Open settings** when it can only be granted there; otherwise Pair asks for it first. |
| Confirm replacement (already paired) | Title **Replace {old pc} with {new pc}?**, buttons **Cancel** · **Replace**. |
| Pairing… | Brackets turn `#8C95FF` and the frame scales to 0.96 (200 ms); a 72 dp dark glass disc (black 72 % + blur) with a 36 dp white spinner sits in the middle; instruction **Pairing…**; back at 40 % and disabled for up to 15 s. |
| Paired | Brackets fade; the square fills with `accent` from the centre (400 ms) with a 56 dp white `check`; instruction **Paired with {pc}**; Home after 800 ms (or immediately on disconnect). |
| Can't reach your PC | Dialog: **Can't reach your PC** / **Make sure both are on the same Wi-Fi, or that Tailscale is connected.** / **Cancel** · **Try again**. |
| Not a Pneu code | Snackbar **That isn't a Pneu code** (3 s); scanning continues; same code ignored for 3 s. |
| Expired code | Snackbar **This code expired. Scan the new one.** |
| PC clock ahead | Snackbar **Your PC's clock says this code expired. Check both clocks and scan the new code.** |
| Camera not granted | Black. Centred column, 22 dp gaps: 72 dp glass disc with `photo_camera` 32 white; primary button **Allow camera**. Requested automatically on first open. |
| Camera denied for good | Same, with **Camera access is off** (`body`, white 75 %) between the disc and the button, which reads **Open settings**. |

A pairing link opened while Pneu is on another screen shows the same confirm dialog over that screen, in the current theme.

### 6.4 Home

Home bar, then (16 dp gutters): optional banner, Link card, Recent.

**Banner** (one at most). Priority: local queue full first, then health issues.

| Case | Text | Action |
|---|---|---|
| Local queue full | **No room for more waiting items** (red tile) | **Don't send any** (clears the waiting items) |
| Several health issues | **{n} settings need attention** | **Fix** → Permissions and battery |
| Local network denied | **Pneu can't connect to your PC** | **Fix** |
| Photos missing or limited | **Pneu can't see new screenshots** | **Fix** |
| System kill detected | **Pneu was stopped in the background** | **Fix** |
| Battery optimisation on | **Battery optimization can stop Pneu** | **Fix** |
| Not in Never sleeping apps (Samsung) | **Pneu isn't in Never sleeping apps** | **Fix** |
| Notifications off | **Notifications are off** | **Fix** |

**Link card states** (precedence: Pairing lost, then Paused, then the connection state; unchanged from v1):

| State | Link | Title | Detail | Buttons |
|---|---|---|---|---|
| Connecting | Connecting… | **Connecting…** | `{pc}` | Send clipboard (disabled) · Pause |
| Connected, Wi-Fi | Connected | **Connected** | `{pc} · Wi-Fi` | Send clipboard · Pause |
| Connected, mobile data | Connected | **Connected** | `{pc} · Mobile data` | Send clipboard · Pause |
| Connected, Tailscale | Connected | **Connected** | `{pc} · Tailscale` | Send clipboard · Pause |
| Sending (any connected route) | Sending | as connected | as connected | as connected |
| PC unreachable | PC unreachable | **PC unreachable** | **Last connected {time}** (omitted if never) | Send clipboard (disabled) · Pause |
| PC unreachable, Tailscale off | PC unreachable | **PC unreachable** | **Tailscale isn't connected** | **Open Tailscale** · Pause |
| Waiting for Wi-Fi | Waiting for Wi-Fi | **Waiting for Wi-Fi** | **Mobile data is off in settings** | Send clipboard (disabled) · Pause |
| No network | No network | **No network** | **Last connected {time}** | Send clipboard (disabled) · Pause |
| Paused | Paused | **Paused** | **Automatic sending is off** | Send clipboard (enabled when connected) · **Resume** (primary) |
| Pairing lost | Pairing lost | **Pairing lost** | **{pc} doesn't recognize this phone** | **Pair again** (primary, full width) |

Button icons: Send clipboard `content_paste_go`, Pause `pause`, Resume `play_arrow`, Open Tailscale `open_in_new`, Pair again `qr_code_scanner`. Behaviour as v1 (Send clipboard reads directly; Pause/Resume immediate, no confirmation).

**Recent** (up to 50 rows; waiting and sending rows on top in send order):

| Item | Title | Thumbnail / tile |
|---|---|---|
| Screenshot | **Screenshot** | image |
| Camera photo | **Photo** | image |
| Screen recording | **Screen recording** | image + play badge |
| Shared file | file name (middle ellipsis) | image if it has a preview, else `description` |
| Clipboard image | **Image** | image, else `image` |
| Clipboard or shared text | first line | `content_paste` |
| Shared link | URL without `https://` | `link` |

| Status | Second line | Trailing |
|---|---|---|
| Sent | `{time}`; clipboard rows **Clipboard · {time}** | none |
| Waiting | **Waiting** | `close`, **Don't send** |
| Held for Wi-Fi | **Waiting for Wi-Fi** | `close`, **Don't send** |
| Sending (> 1 s) | **Sending · {sent} of {total}** in `accentText` + progress bar | `close`, **Cancel** |
| Failed | in `danger`: **Failed · PC disk is full** / **Failed · PC can't save files** / **Failed · PC can't receive this** / **Failed · Couldn't read image** / **Failed · File is gone** / **Failed** | `refresh` **Retry**, `close` **Remove** |

Header menu ("{n} waiting"): **Send now** (retry the connection) and **Don't send any** (in `danger`; empties the queue, no confirmation). Tap behaviour on rows is unchanged (sent automatic images/videos open the original; sent links open the browser; others do nothing).

**Action errors** (any setting or action that fails to save): dialog with the failure as title, for example **Couldn't pause**, **Couldn't resume**, **Couldn't save setting**, **Couldn't clear queue**, **Couldn't retry item**, **Couldn't remove item**, **Couldn't retry connection**, **Couldn't start pairing**, **Couldn't unpair**, **Couldn't save setup**, **Couldn't save battery settings**, **Couldn't acknowledge background stop**, **Couldn't request camera access**, **Couldn't cancel pairing**, **Couldn't start connection**. Buttons **Close** · **Try again** when a retry exists; otherwise one **Close**.

### 6.5 Settings

Large title **Settings** with back. Content (16 dp gutters):

1. **Paired PC card** (no header): row 76 dp with a 44 dp tile (radius 14, `accentSoft`, `computer`), title `{pc}` in `headline`, second line the known addresses `192.168.1.20 · 100.67.144.28`. Pairing lost: tile `dangerSoft` with `link_off`, second line **Pairing lost** in `danger`. Not paired: title **Not paired**. Then a row **Unpair** in `danger` → Unpair dialog.
2. **Send automatically**: Screenshots (on), Camera photos (off), Screen recordings (off).
3. **Clipboard**: Send button in notification (on), Receive clipboard from PC (on).
4. **Connection**: Direct on home Wi-Fi (on); Tailscale when away (on; second line **Tailscale app isn't installed** in `danger` when missing); Use mobile data (on); Send originals on mobile data (off; disabled while Use mobile data is off); Queue while PC is unreachable (on).
5. **This phone**: Show in share sheet (on), Quick Settings tiles (on), **Permissions and battery** (navigation row; second line **All set** in `text2`, or **{n} needs attention** / **{n} need attention** in `warn`).
6. Footer **Version {versionName}**, `caption`, `text3`, centred, 24 dp padding.

Rules unchanged: turning on Camera photos or Screen recordings asks for media access first; turning off the last of Wi-Fi and Tailscale leaves it on with snackbar **Pneu needs Wi-Fi or Tailscale**.

**Unpair dialog**: title **Unpair from {pc}?** (or **Finish unpairing?** when the PC's pin is already gone); text **{n} waiting items won't be sent.** / **1 waiting item won't be sent.** only when items wait; buttons **Cancel** · **Unpair** (destructive). Behaviour unchanged.

### 6.6 Status notification, alerts, tiles

System-rendered. Pneu supplies `ic_stat_pneu`, accent colour `#4F5BFF` (`setColor`), title, text, actions and progress. Channels, importance, throttling and all copy are unchanged from v1 3.7 and 3.8, plus the implemented additions:

| Notification | Title | Text | Actions |
|---|---|---|---|
| Settings loading (service start) | **Loading Pneu settings** | none | none |
| Settings failed to load | **Couldn't load Pneu settings** | none | none |
| Sending a clipboard image | **Sending clipboard image** | **{sent} of {total}** + progress | as status |
| Share admission (foreground while copying shares) | **Preparing share** | none | none |
| Pairing lost (alert) | **Pairing lost** | **{pc} doesn't recognize this phone** | **Pair again** |
| Local network off (alert) | **Pneu can't connect to your PC** | **Local network access is off** | **Fix** |
| Stopped in background (alert) | **Pneu was stopped in the background** | **Add Pneu to Never sleeping apps** on Samsung when that list isn't satisfied, otherwise **Check permissions and battery settings** | **Fix** |

Quick Settings tiles: icon `ic_stat_pneu` (Pneu) and `content_paste_go` (Send clipboard); labels and subtitles unchanged (Pneu: **Wi-Fi** / **Tailscale** / **Mobile data** / **Connecting…** / **Offline** / **Paused** / **Pairing lost** / **Not paired**; Send clipboard: none or **Offline**, **Not paired**). Tile colours are One UI's.

### 6.7 Clipboard, share target, toasts

No Pneu UI (translucent activities). Toasts are system-styled. Copy unchanged, plus implemented additions:

| Situation | Toast |
|---|---|
| Clipboard sent | **Clipboard sent** |
| Clipboard empty | **Clipboard is empty** |
| Not connected | **PC unreachable. Not sent.** |
| Sensitive clip | **Sensitive clipboard. Not sent.** |
| Clipboard send interrupted (process restored) | **Clipboard send interrupted. Send again.** |
| Shared, connected | **Sending to PC** / **Sending {n} items to PC** |
| Shared, PC unreachable | **PC unreachable. Queued.** |
| Shared, waiting for Wi-Fi | **Waiting for Wi-Fi. Queued.** |
| Item unreadable | **Couldn't read this item** |
| Share interrupted (process restored) | **Sharing interrupted. Share again.** |
| Settings unavailable | **Couldn't load Pneu settings. Open Pneu and try again.** |
| Pause from tile or notification failed | **Couldn't change pause. Open Pneu and try again.** |

Share target label **Send to PC**, app icon.

---

## 7. PC (KDE Plasma 6.6)

### 7.1 Tray

States, SNI status, tooltip text, left-click behaviour and the menu are unchanged from v1 4.1–4.2. Icon per state:

| State | Icon |
|---|---|
| No phone paired, pairing in progress, phone not connected | `pneu-tray-disconnected` |
| Connected | `pneu-tray-connected` |
| Receiving (≥ 1.5 s) | `pneu-tray-receiving` |
| Phone paused | `pneu-tray-paused` |
| Error | `pneu-tray-error` (SNI NeedsAttention) |

Menu (Plasma-rendered; structure and labels unchanged):

```
{status line}                       disabled; same as the tooltip's first line
──────────────
Open Folder                         folder-open
──────────────
✓ Save to Folder
✓ Show Notifications
✓ Copy Images to Clipboard
  Open in Image Viewer
──────────────
✓ Send Clipboard to Phone
  Include Images                    disabled while Send Clipboard to Phone is off
──────────────
Configure Pneu… / Pair Phone…       configure / smartphone
Quit                                application-exit
```

Tooltip: title **Pneu**, icon `pneu`, text as v1 (for example **Galaxy S25+ connected over Wi-Fi**; **Galaxy S25+ isn't connected** + **Last seen {time}**).

### 7.2 Window

Title **Pneu**, icon `pneu`, 400 logical px wide, height fits content, not resizable, one instance. Opens from the tray and on its own when the daemon starts unpaired. Closing (title-bar ✕ or Esc) hides it and invalidates its displayed token as today. **No Close button.**

Layout: 16 px padding, 12 px between blocks.

**Unpaired**

1. QR card: white, radius 16, 14 px padding, soft shadow (light) or 1 px white 6 % border (dark), centred. QR 232 px, black on white, error correction M, 4-module quiet zone, refreshed every 5 minutes with no countdown (unchanged).
2. **Scan with Pneu on your phone**, 14 px, centred.
3. Settings card with one row: label **Save to** (12.5 px `text2`) over the path (middle-elided, `~` for home), **Change…** button at the end.

| Unpaired variant | QR area (260 px tall) |
|---|---|
| Pairing in progress | 30 px spinner (`accent` on `accentSoft`) above **Pairing with {phone}…** |
| No usable address | 44 px monitor-with-slash symbol in `text2` above **Connect this PC to a network to pair** |

**Paired**

1. Status card (radius 14, padding 16 / 14 / 14, centred): the Link at PC scale (40 px tiles, 3 px tube), 12 px, `{phone}` 15 px bold, status line in `text2`.
2. Settings card: **Save to** row as above; separator; row **Unpair…** in `danger`, semibold, full-row button.

| Status | Link | Status line |
|---|---|---|
| Connected | tiles accentSoft, solid gradient tube | **Connected over Wi-Fi** / **Connected over Tailscale** |
| Receiving | tiles accentSoft, tube `cardBorder`, capsule running | **Receiving…** |
| Paused on phone | phone tile warnSoft/warn, PC tile neutral, `pause` badge | **Paused on phone** |
| Not connected | neutral tiles, dashed tube | **Not connected · last seen {time}** or **Not connected** |

**Error banner** (above everything while the error lasts): `dangerSoft`, 1 px `#DA4453` at 35 %, radius 10, padding 10 × 12, `error` 16 px in `#DA4453`, text: **Port {port} is in use by another program.** / **Can't write to {folder}.** / **The disk with {folder} is full.** No button.

**Unpair popup**: window content dimmed (`dim`); centred card 320 px, radius 14, padding 20, `card` with `cardBorder` and a large soft shadow. Title **Unpair {phone}?** 15 px bold. Buttons right-aligned, 8 px apart: **Cancel** (neutral, focused by default) · **Unpair** (`dangerFill`, white). Esc or Enter on Cancel closes it; behaviour unchanged.

**Buttons**: 30 px tall, radius 8, 14 px side padding, base font. Neutral: `button` fill with 1 px `buttonBorder`; hover lightens/darkens fill by 4 %; pressed by 8 %. Keyboard focus: 1 px `accent` border + 2 px `accent` 35 % outer ring. Tab order: Change…, Unpair…; Space/Enter activate.

**Folder picker**: unchanged (XDG portal, title **Choose Folder**).

### 7.3 Notifications

Unchanged from v1 4.4 (summaries, bodies, actions, hints, urgency, expiry, `x-kde-urls` previews), with the new `pneu` icon. Implemented additions keep their copy: **Image from phone** (explicit clipboard-image thumbnail), **Couldn't open folder** (+ the error), **Couldn't copy image**.

---

## 8. Strings

All phone strings in v1 3.11 stay. Strings the app already uses beyond v1 are specified above (6.1–6.7) and are unchanged in wording. Added or changed in v2:

| Key | Text | Note |
|---|---|---|
| `pair_title` | Pair with PC | Now the pane title only, not drawn |
| `cd_back` | Back | Back buttons |
| `cd_granted` | Granted | Done permission rows |
| `perm_local_network` | Local network | Existing row, now a resource |
| `pref_clipboard_from_pc` | Receive clipboard from PC | Existing row, now a resource |
| `pref_share_sheet` | Show in share sheet | Existing row, now a resource |
| `pref_quick_settings` | Quick Settings tiles | Existing row, now a resource |
| `pair_confirm_title` | Pair with %1$s? | Existing dialog |
| `pair_replace_title` | Replace %1$s with %2$s? | Existing dialog |
| `action_pair` / `action_replace` / `action_dismiss` / `action_close` | Pair / Replace / Dismiss / Close | Existing buttons |
| `unpair_finish_title` | Finish unpairing? | Existing dialog title |
| `banner_capacity` | No room for more waiting items | Existing banner |
| `banner_local_network` | Pneu can't connect to your PC | Existing banner |
| `load_failed` | Couldn't load Pneu settings | Existing |
| `kind_image` | Image | Clipboard-image rows |
| `detail_mobile` | %1$s · Mobile data | Existing route label |

Move every hard-coded string in `MainActivity.kt`, `PneuService.kt`, `features/*` into `strings.xml` while implementing (wording unchanged).

PC strings: unchanged from v1 4.6, minus the window's **Close** button label.

---

## 9. Implementation notes

### 9.1 Android (Compose)

**Theme**
- Replace `dynamicLightColorScheme`/`dynamicDarkColorScheme` with `PneuTheme(dark = isSystemInDarkTheme())` that provides an `@Immutable data class PneuColors` (every token in 4.1) through `staticCompositionLocalOf`, plus `PneuType` (4.3, `FontFamily.Default`, letter spacing in sp) and shapes.
- Also build a matching M3 `ColorScheme` (`primary = accent`, `onPrimary = white`, `primaryContainer = accentSoft`, `onPrimaryContainer = onAccentSoft`, `secondaryContainer = fill`, `surface = surface`, `background = bg`, `surfaceContainerHigh = raised`, `error = danger`, `errorContainer = dangerSoft`, `outline = fill2`, `inverseSurface = inverse`) so any remaining M3 parts (snackbar host, menus, text selection, scroll indicators) agree.
- Edge-to-edge as today; `enableEdgeToEdge(SystemBarStyle.auto(Transparent, Transparent))`; the Pair screen forces light-on-dark bars (already done via `WindowCompat`).

**Components** (one file each under `ui/`; no extra abstraction layers):

| Spec | Compose |
|---|---|
| Card | `Surface(shape = RoundedCornerShape(24.dp), color = c.surface, border = if (dark) BorderStroke(1.dp, c.hairline) else null)` |
| Rows | `Row(Modifier.heightIn(min = 56/68.dp).clickable(indication = null, …).padding(…))`; separators drawn with `drawBehind` from the second row; pressed overlay via `interactionSource.collectIsPressedAsState()` |
| Switch | Custom `PneuSwitch` (`Modifier.toggleable(role = Role.Switch)`, `animateDpAsState` for the thumb, `animateColorAsState` for the track). M3 `Switch` thumb sizes differ by state, so don't use it. |
| Buttons | `PneuButton(kind, text, icon, enabled, onClick)` on `Surface(onClick, shape = RoundedCornerShape(14.dp))`; press scale via `graphicsLayer { scaleX = s; scaleY = s }` with `animateFloatAsState(if (pressed) .97f else 1f, tween(120))`; no ripple |
| Large title bar | `LargeTopAppBar` with `TopAppBarDefaults.exitUntilCollapsedScrollBehavior()`, `colors(containerColor = bg, scrolledContainerColor = bg)`, title style switching from `largeTitle` to `title2` via the collapsed fraction |
| Link | `LinkView(state: LinkState, sending: Boolean)`: two 52 dp `Box` tiles and a `Canvas` tube. Dashes: `PathEffect.dashPathEffect(floatArrayOf(6dp, 7dp), phase)` with `phase` from `rememberInfiniteTransition` (800 ms linear). Capsule: `Box` offset by an infinite transition (1400 ms, `CubicBezierEasing(.45f, 0f, .2f, 1f)`) plus alpha keyframes. Badge: `Box` with `Modifier.border(4.dp, c.surface, CircleShape)`. Mark it `clearAndSetSemantics {}`. |
| Reduced motion | `val animate = Settings.Global.getFloat(cr, ANIMATOR_DURATION_SCALE, 1f) > 0f`; when false, no infinite transitions (capsule centred, dashes static). |
| Banner | `Row` in a `Surface(shape = 20.dp)`; text `maxLines = 2` |
| Recent progress | `LinearProgressIndicator(progress = { … }, Modifier.height(3.dp).clip(RoundedCornerShape(2.dp)), color = c.accent, trackColor = c.fill2, gapSize = 0.dp, drawStopIndicator = {})` |
| Menu | `DropdownMenu(shape = RoundedCornerShape(16.dp), containerColor = c.raised, shadowElevation = 8.dp, border = hairline-in-dark)` |
| Dialog | `PneuDialog`: `Dialog(properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false))` containing a full-size `Box(contentAlignment = BottomCenter)` with the scrim and the card (`navigationBarsPadding().padding(12.dp)`), entry via `AnimatedVisibility(slideInVertically { it / 8 } + fadeIn(tween(280, easing = easeOut)))`. Replaces every `AlertDialog`. |
| Snackbar | `SnackbarHost { Snackbar(it, shape = RoundedCornerShape(14.dp), containerColor = c.inverse, contentColor = c.onInverse) }` |
| Pair brackets | `Canvas` drawing four arcs + lines (stroke 4 dp, `StrokeCap.Round`) around the cut-out; colour animates to `#8C95FF` while busy; success fill is an `animateFloatAsState` scale of a rounded `drawRoundRect`. |
| Predictive back | Keep the current screen state machine; wrap screen changes in `AnimatedContent` and handle back with `PredictiveBackHandler` (progress → scale 1 → 0.94 and 0 → 24 dp corner radius on the leaving screen). |

**Icons and resources**
- Launcher: `mipmap-anydpi-v26/ic_launcher.xml` keeps background/foreground/monochrome. Import each layer SVG with Vector Asset Studio (it converts the gradients); or write them by hand:
  - Background: two `<path android:pathData="M0,0h108v108h-108z">` elements. The first has a linear `aapt:attr` gradient (`startX 32.4 startY 0 endX 75.6 endY 108`, `<item android:offset="0" android:color="#FF6B74FF"/>`, `0.55 #FF4846E8`, `1 #FF2620AE`). The second has a radial gradient (`centerX 54 centerY -8.64 gradientRadius 84.24`, `0 #52FFFFFF`, `1 #00FFFFFF`).
  - Foreground: one `<path>` per SVG path, in order. Stroked paths use `android:strokeColor` (or an `aapt:attr name="android:strokeColor"` gradient), `strokeWidth`, `strokeAlpha`, `strokeLineCap="round"`, `strokeLineJoin="round"`, `fillColor="#00000000"`. Filled paths use `fillColor`/`fillAlpha` or a gradient. Per-stop opacity becomes the alpha byte of the stop colour (95 % white = `#F2FFFFFF`).
  - Monochrome: the small-master path, `strokeWidth="9.36"`, black.
- `ic_stat_pneu`: the small master from `ic_stat_pneu.svg` (`strokeWidth="3.4"`, white).
- Home top bar mark: import `pneu-mark.svg` as `ic_pneu_mark`.
- Notification colour: `setColor(0xFF4F5BFF.toInt())` (was teal).

### 9.2 PC (Slint)

- Stop importing `Button` from `std-widgets.slint`. Keep the Qt backend (for `Palette` and the Plasma scheme), but draw Pneu components. Add a `global Pneu` with `property <bool> dark: Palette.color-scheme == ColorScheme.dark;` and one property per token in 4.2 (`card: dark ? #2A2E32 : #FFFFFF;` …). Window `background: Palette.background`.
- Components in the same `slint!` block: `PneuButton` (Rectangle + TouchArea + FocusScope; `kind` neutral/danger; hover/pressed via `touch.has-hover`/`touch.pressed`; focus ring when `fs.has-focus`), `Card` (Rectangle radius 14, border 1 px), `Row`, `Link` (two Rectangles radius 12 with `Image { colorize: … }`, a 3 px tube: solid gradient `@linear-gradient(90deg, #7381FF 0%, #4F5BFF 100%)`, dashed as a `HorizontalLayout` of `for i in 18: Rectangle { width: 5px; }` with 6 px spacing, capsule `Rectangle` whose `x` animates with `animate x { duration: 1400ms; easing: cubic-bezier(0.45, 0, 0.2, 1); iteration-count: -1; }` while receiving).
- Map from v1: `Spinner` stays (std-widgets) but inside the 260 px QR area; the status dot is replaced by the Link; `Close` button and its Enter handling are removed (Esc still hides; the window's default focus moves to **Change…**); `Unpair…` becomes the last row of the settings card; the existing `PopupWindow` confirm keeps its logic, restyled per 7.2.
- Icons: ship `smartphone`, `computer`, `pause`, `error`, monitor-with-slash as SVG next to `pneu.svg` and colourise in Slint; stop relying on the icon theme for the window (the existing recolouring code for `phone-icon`/`network-icon` can go).
- Window icon: `@image-url(".../assets/pneu.svg")` now points at the v2 file.

### 9.3 Tray and notifications

- Replace the five files in `desktop/assets/` with the v2 tray SVGs (same names). The renderer already substitutes `currentColor`; the receiving file's fixed `#6370FF` and the badge classes need no code change. Confirm resvg honours `stroke-opacity` for the disconnected state (it does in current resvg).
- Replace `pneu.svg` (app icon) in `desktop/assets/` and the installer's hicolor target.
- Notifications: no code change beyond the icon.

### 9.4 Verification

- Phone: check every state in `index.html` on the S25+ in light and dark at font scale 1.0 and 1.3 (buttons stack), with animations on and off.
- PC: check the window in Breeze Light and Breeze Dark and one custom scheme; tray icons on light and dark panels at 22 px and 16 px.

---

## 10. Decisions (resolved)

1. **Icon**: A · Monogram, refined as in section 3 (lit tube P, carrier dropping out of the stem; flat P at small sizes).
2. **Palette**: fixed Pneu palette, no dynamic colour.
3. **PC window**: Pneu components, following Plasma light/dark.
4. **Close button**: removed; title-bar ✕ and Esc close the window.
5. **Font**: system fonts on both sides; nothing bundled.
