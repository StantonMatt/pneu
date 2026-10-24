//! Accessibility preferences, read without KDE configuration on macOS.
pub fn globals() -> String {
    if super::macos::native::reduced_motion() {
        "[KDE]\nAnimationDurationFactor=0\n".into()
    } else {
        String::new()
    }
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
