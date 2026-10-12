//! Resolve the panel theme separately from the application window palette.
use std::path::PathBuf;
#[derive(Clone, Debug)]
pub struct Colors {
    pub foreground: String,
    pub negative: String,
    pub background: String,
}
pub fn config_root() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}
pub fn globals() -> String {
    std::fs::read_to_string(
        std::env::var_os("PNEU_KDEGLOBALS")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_root().join("kdeglobals")),
    )
    .unwrap_or_default()
}
pub fn value<'a>(ini: &'a str, group: &str, key: &str) -> Option<&'a str> {
    let mut active = false;
    for line in ini.lines().map(str::trim) {
        if line.starts_with('[') {
            active = line == format!("[{group}]");
        } else if active
            && let Some((name, value)) = line.split_once('=')
            && name == key
        {
            return Some(value);
        }
    }
    None
}
fn color(ini: &str, key: &str) -> Option<String> {
    let rgb: Vec<u8> = value(ini, "Colors:Window", key)?
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    (rgb.len() == 3).then(|| format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]))
}
pub fn colors(ini: &str) -> Colors {
    Colors {
        foreground: color(ini, "ForegroundNormal").unwrap_or_else(|| "#232629".into()),
        negative: color(ini, "ForegroundNegative").unwrap_or_else(|| "#da4453".into()),
        background: color(ini, "BackgroundNormal").unwrap_or_else(|| "#eff0f1".into()),
    }
}
pub fn window_colors() -> Colors {
    colors(&globals())
}
pub fn panel_colors() -> Colors {
    let config = std::fs::read_to_string(config_root().join("plasmarc")).unwrap_or_default();
    let mut roots = vec![
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            }),
    ];
    roots.extend(std::env::split_paths(
        &std::env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
    ));
    panel_colors_in(
        &globals(),
        value(&config, "Theme", "name").unwrap_or("default"),
        &roots,
    )
}
fn panel_colors_in(globals: &str, theme: &str, roots: &[PathBuf]) -> Colors {
    if !theme.contains('/') && !theme.contains("..") {
        for root in roots {
            if let Ok(ini) =
                std::fs::read_to_string(root.join("plasma/desktoptheme").join(theme).join("colors"))
            {
                return colors(&ini);
            }
        }
    }
    // Adaptive themes, including Breeze's default, inherit the window scheme.
    colors(globals)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn light_dark_and_custom_panel_palettes() {
        let light = "[Colors:Window]\nForegroundNormal=35,38,41\nBackgroundNormal=239,240,241\n[Colors:Complementary]\nForegroundNormal=252,252,252\n";
        let dark = "[General]\nColorScheme=CustomMidnight\n[Colors:Window]\nBackgroundNormal=20,22,24\nForegroundNormal=240,241,242\n";
        assert_eq!(colors(light).foreground, "#232629");
        assert_eq!(colors(dark).foreground, "#f0f1f2");
        assert_eq!(colors(dark).background, "#141618");
        let dir = tempfile::tempdir().unwrap();
        let theme = dir.path().join("plasma/desktoptheme/custom-panel");
        std::fs::create_dir_all(&theme).unwrap();
        std::fs::write(theme.join("colors"), dark).unwrap();
        assert_eq!(
            panel_colors_in(light, "custom-panel", &[dir.path().into()]).foreground,
            "#f0f1f2"
        );
        assert_eq!(
            panel_colors_in(light, "default", &[dir.path().into()]).foreground,
            "#232629"
        );
    }
}
