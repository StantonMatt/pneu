//! Only the animation preference is consumed by the existing window.
pub fn globals() -> String {
    String::new()
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
