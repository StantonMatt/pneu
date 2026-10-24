# macOS assets

Drawn from the Crossing mark (`docs/design/v2/DESIGN.md`, section 3).

| File | In the bundle | Use |
|---|---|---|
| `menubar-<state>.png`, `menubar-<state>@2x.png` | `Contents/Resources/menubar/` | Menu bar icon, 18 × 18 pt template image (black plus alpha; set `isTemplate`). States: `connected`, `disconnected`, `error`, `paused`, `receiving`. |
| `menubar-<state>.svg` | not bundled | Sources for the PNGs. |
| `AppIcon.iconset/` | `Contents/Resources/AppIcon.icns` | `iconutil -c icns AppIcon.iconset -o AppIcon.icns`; `CFBundleIconFile` = `AppIcon`. |
| `AppIcon.svg` | not bundled | Master for every iconset PNG except `icon_16x16.png`: the macOS rounded square (824 on a 1024 canvas, 100 margin) with a drop shadow. |
| `AppIcon-16.svg` | not bundled | Master for `icon_16x16.png`. The ghost is one continuous outline because dashes can't resolve at 16 px. |

`menubar-receiving` is a copy of `menubar-connected`: the menu bar icon doesn't change while sending or receiving. `menubar-disconnected` is the same glyph at 50 % alpha, which template rendering keeps.
