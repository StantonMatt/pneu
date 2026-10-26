//! Win32 component rules, shared by strict path validation and receive mapping.

fn forbidden(c: char) -> bool {
    c < ' ' || matches!(c, '"' | '*' | '/' | ':' | '<' | '>' | '?' | '\\' | '|')
}

fn device(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

pub(crate) fn is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.encode_utf16().count() <= 255
        && !name.chars().any(forbidden)
        && !name.ends_with(['.', ' '])
        && !device(name)
}

pub(crate) fn map(name: &str, max: usize) -> String {
    assert!(max >= 32, "receive name budget must fit an extension");
    let max = max.min(255);
    let mut name: String = name
        .chars()
        .map(|c| if forbidden(c) { '_' } else { c })
        .collect();
    loop {
        name.truncate(name.trim_end_matches(['.', ' ']).len());
        if device(&name) {
            name.insert(0, '_');
        }
        if name.is_empty() {
            // Only remote receive names are rewritten; configured paths stay strict.
            name.push_str("file");
        }
        if name.len() <= max {
            return name;
        }
        // Keep the existing extension-aware byte budget, then recheck any
        // trailing dot/space or device stem exposed by truncation.
        name = super::safe_basename(&name, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_rules_and_unchanged_names() {
        for (wire, local) in [
            ("CON.txt", "_CON.txt"),
            ("nul", "_nul"),
            ("COM¹.log", "_COM¹.log"),
            ("CON .txt", "_CON .txt"),
            ("a:b.jpg", "a_b.jpg"),
            ("notes.", "notes"),
            ("what?.png", "what_.png"),
            ("a<>:\"/\\|?*b", "a_________b"),
            ("a\0\u{1}\u{1f}b", "a___b"),
            ("notes . .  ", "notes"),
            ("", "file"),
            (". .  ", "file"),
            (" leading space.txt", " leading space.txt"),
            ("café-写真.png", "café-写真.png"),
            (".hidden", ".hidden"),
            ("COM0.txt", "COM0.txt"),
            ("COM10.txt", "COM10.txt"),
            ("CONSOLE.txt", "CONSOLE.txt"),
            ("a\u{7f}b", "a\u{7f}b"),
        ] {
            assert_eq!(map(wire, 255), local, "{wire:?}");
            assert!(is_safe(local), "{local:?}");
            assert_eq!(map(local, 255), local);
        }
    }

    #[test]
    fn strict_rules_keep_the_existing_utf16_limit() {
        for name in ["", ".", "..", "data.", "data ", "a:b", "CON .txt"] {
            assert!(!is_safe(name), "{name:?}");
        }
        assert!(is_safe(&"界".repeat(255)));
        assert!(!is_safe(&"界".repeat(256)));
        assert!(is_safe(&format!("{}a", "😀".repeat(127))));
        assert!(!is_safe(&"😀".repeat(128)));
    }

    #[test]
    fn nasty_names_are_safe_bounded_and_idempotent() {
        let mut names: Vec<String> = [
            "",
            ".",
            "..",
            "...",
            " ",
            "   ",
            ". . .  ",
            " leading",
            "x.  .. ",
            "a<>:\"/\\|?*b",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        names.extend((0..32).map(|c| format!("a{}b", char::from(c))));
        for c in (0..32).map(char::from).chain("<>:\"/\\|?*".chars()) {
            let wire = format!("a{c}b");
            assert!(!is_safe(&wire));
            assert_eq!(map(&wire, 255), "a_b");
        }
        let mut devices: Vec<String> = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        for prefix in ["COM", "LPT"] {
            for suffix in ["1", "2", "3", "4", "5", "6", "7", "8", "9", "¹", "²", "³"] {
                devices.push(format!("{prefix}{suffix}"));
            }
        }
        for device in devices {
            // Every ASCII case permutation, with and without extensions/spaces.
            for mask in 0..1 << device.len() {
                let mixed: String = device
                    .chars()
                    .enumerate()
                    .map(|(i, c)| {
                        if mask & (1 << i) != 0 {
                            c.to_ascii_lowercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                for ext in ["", ".txt", " .txt", ".tar.gz", "...  "] {
                    let wire = format!("{mixed}{ext}");
                    assert!(!is_safe(&wire), "{wire:?}");
                    assert!(map(&wire, 255).starts_with('_'), "{wire:?}");
                    names.push(wire);
                }
            }
        }
        names.extend([
            format!("CON.{}", "x".repeat(251)),
            format!("{} .png", "界".repeat(83)),
            format!("{}xx.png", "界".repeat(83)),
            format!("{}abc", "😀".repeat(63)),
            format!("{} {}", "a".repeat(254), "b".repeat(32)),
            format!("CON{}x", " ".repeat(252)),
            "界".repeat(255),
            "😀".repeat(256),
        ]);
        for name in names {
            for max in [32, 200, 255] {
                let local = map(&name, max);
                assert!(is_safe(&local), "{name:?} -> {local:?}");
                assert!(local.len() <= max, "{local:?}");
                assert!(local.encode_utf16().count() <= 255);
                assert_eq!(map(&local, max), local, "{name:?}");
            }
        }
    }

    #[test]
    fn truncation_preserves_extensions_and_remaps_exposed_suffixes() {
        assert_eq!(map(&format!("CON.{}", "a".repeat(251)), 255).len(), 255);
        assert_eq!(map(&format!("{} x", "a".repeat(199)), 200), "a".repeat(199));
        let long = format!("{}xx.png", "界".repeat(83));
        assert_eq!(long.len(), 255);
        let local = map(&long, 200);
        assert!(local.ends_with(".png"));
        assert!(local.len() <= 200);
    }
}
