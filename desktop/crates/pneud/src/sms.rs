//! Ephemeral SMS presentation and deliberately conservative one-time-code detection.
//! No message bodies or codes enter the transfer store or diagnostic output.
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_AGE_MS: u64 = 120_000;
pub const CLEAR_AFTER_SECS: u64 = 60;

#[derive(Clone, PartialEq, Eq)]
pub struct Code {
    pub digits: String,
    pub ranges: Vec<Range<usize>>,
}
fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            _ => c,
        })
        .filter(|c| !matches!(*c, '\u{0300}'..='\u{036f}'))
        .collect()
}
fn words(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
fn keyword(word: &str) -> bool {
    matches!(
        word,
        "codigo"
            | "cod"
            | "clave"
            | "contrasena"
            | "contraseña"
            | "pin"
            | "otp"
            | "token"
            | "verificacion"
            | "autenticacion"
            | "code"
            | "passcode"
            | "password"
            | "verification"
            | "one"
    )
}
fn amount(word: &str) -> bool {
    matches!(word, "clp" | "uf" | "pesos" | "puntos" | "pts" | "us")
}
fn fragment(word: &str) -> bool {
    matches!(
        word,
        "cuenta" | "tarjeta" | "card" | "account" | "nro" | "terminada" | "ending"
    )
}

/// Exactly one distinct 4–8 digit candidate within six words of a code word.
/// Scanning whole numeric expressions prevents recovering a suffix of a phone,
/// date, amount or RUT after rejecting its prefix.
pub fn detect(text: &str) -> Option<Code> {
    let bytes = text.as_bytes();
    let all_words: Vec<_> = text
        .split_word_bound_indices()
        .filter(|(_, w)| w.chars().any(char::is_alphanumeric))
        .collect();
    let keys: Vec<usize> = all_words
        .iter()
        .enumerate()
        .filter_map(|(i, (_, w))| {
            let folded = fold(w);
            let postal = all_words
                .get(i.wrapping_sub(1))
                .is_some_and(|(_, s)| fold(s) == "postal")
                || all_words
                    .get(i + 1)
                    .is_some_and(|(_, s)| fold(s) == "postal");
            (keyword(&folded)
                && !postal
                && (folded != "one"
                    || all_words.get(i + 1).is_some_and(|(_, s)| fold(s) == "time")))
            .then_some(i)
        })
        .collect();
    if keys.is_empty() {
        return None;
    }
    let mut candidates: Vec<String> = Vec::new();
    let mut expressions = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let mut segments = vec![i - start];
        let mut punctuation = false;
        while i + 1 < bytes.len()
            && matches!(bytes[i], b'-' | b' ' | b'.' | b',' | b'/' | b':')
            && bytes[i + 1].is_ascii_digit()
        {
            let separator = bytes[i];
            // A separated number that exceeds eight digits is a phone/amount,
            // not a smaller code followed by an unrelated suffix.
            punctuation |= matches!(separator, b'.' | b',' | b'/' | b':');
            i += 1;
            let part = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            segments.push(i - part);
        }
        let end = i;
        let raw = &text[start..end];
        let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
        expressions.push((digits.clone(), start..end));
        if !(4..=8).contains(&digits.len()) || segments.len() > 3 || punctuation {
            continue;
        }
        // Two short parts that form a day and month ("27-11") are a date, not a code.
        if segments.len() == 2 && segments[0] <= 2 && segments[1] <= 2 {
            let (a, b): (u32, u32) = (
                digits[..segments[0]].parse().unwrap_or(0),
                digits[segments[0]..].parse().unwrap_or(0),
            );
            let day_month = |d: u32, m: u32| (1..=31).contains(&d) && (1..=12).contains(&m);
            if day_month(a, b) || day_month(b, a) {
                continue;
            }
        }
        if raw.contains('-')
            && segments.len() == 3
            && ((segments[0] <= 2 && segments[1] <= 2 && matches!(segments[2], 2 | 4))
                || (segments[0] == 4 && segments[1] <= 2 && segments[2] <= 2))
        {
            continue;
        }
        let before = &text[..start];
        let after = &text[end..];
        let previous = before.trim_end();
        if previous.ends_with(['+', '*', '$', '€', '£', '#', '%', '°'])
            || after.trim_start().starts_with(['%', '$', '€', '£'])
        {
            continue;
        }
        let prev_char = before.chars().next_back();
        if prev_char.is_some_and(char::is_alphanumeric)
            || after.chars().next().is_some_and(char::is_alphabetic)
        {
            continue;
        }
        let left = words(before);
        let right = words(after);
        if left.last().is_some_and(|w| amount(w)) || right.first().is_some_and(|w| amount(w)) {
            continue;
        }
        if left
            .iter()
            .rev()
            .take(3)
            .any(|w| fragment(w) || matches!(w.as_str(), "postal" | "zip"))
        {
            continue;
        }
        // A leading letter-prefix followed by '-' is accepted; numeric prefixes
        // have already been consumed as part of this expression.
        let position = all_words
            .iter()
            .position(|(at, w)| *at <= start && start < *at + w.len())
            .unwrap_or_else(|| all_words.iter().take_while(|(at, _)| *at < start).count());
        if !keys.iter().any(|key| key.abs_diff(position) <= 7) {
            continue;
        }
        candidates.push(digits);
    }
    let first = candidates.first()?.clone();
    if candidates.iter().any(|value| value != &first) {
        return None;
    }
    Some(Code {
        ranges: expressions
            .into_iter()
            .filter_map(|(value, range)| (value == first).then_some(range))
            .collect(),
        digits: first,
    })
}
pub fn clean(text: &str) -> String {
    text.chars().filter(|c| (!c.is_control() || *c=='\n') && !matches!(*c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')).collect()
}
pub fn masked(text: &str, code: Option<&Code>) -> String {
    text.char_indices()
        .map(|(at, c)| {
            if c.is_ascii_digit()
                && code.is_some_and(|code| code.ranges.iter().any(|r| r.contains(&at)))
            {
                '•'
            } else {
                c
            }
        })
        .collect()
}
pub fn preview(text: &str) -> String {
    let mut result = String::new();
    let mut chars = 0;
    let mut lines = 1;
    for cluster in text.graphemes(true) {
        let count = cluster.chars().count();
        if chars + count > 300 || (cluster.contains('\n') && lines == 4) {
            result.push('…');
            break;
        }
        chars += count;
        if cluster.contains('\n') {
            lines += 1;
        }
        result.push_str(cluster);
    }
    result
}
pub fn fresh(received_at: u64, now: u64) -> bool {
    received_at <= now.saturating_add(60_000) && now.saturating_sub(received_at) < MAX_AGE_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approved_examples_and_chilean_exclusions() {
        for (text, expected) in [
            (
                "BancoEstado: Tu clave dinámica es 482913. No la compartas.",
                Some("482913"),
            ),
            (
                "Santander: Codigo de verificacion 7731-22. No lo compartas con nadie.",
                Some("773122"),
            ),
            ("Tu código de Uber es 1234.", Some("1234")),
            ("G-482913 is your Google verification code.", Some("482913")),
            (
                "Santander: Código 551204 para autorizar transferencia de $1.500.000 a cuenta *4321",
                Some("551204"),
            ),
            (
                "BancoEstado: Tu clave dinámica es 482913 y vence a las 14:35",
                Some("482913"),
            ),
            (
                "Banco de Chile: Compra por $45.990 con tarjeta terminada en 1234",
                None,
            ),
            ("Entel: Tienes 2500 puntos. Canjéalos en entel.cl", None),
            ("Tu código postal es 8320000", None),
            ("Te llamo a las 1530, mi clave del wifi es 12345678", None),
            ("Tu código es 4821 o 9921", None),
            ("Felicidades! Ganaste un premio, ingresa a bit.ly/x9", None),
            ("OTP 482 913", Some("482913")),
            ("Código 482913. Repito código 482913", Some("482913")),
            (
                "Código 482913 para entrar. No lo compartas con ninguna otra persona. Repito: 482913.",
                Some("482913"),
            ),
            ("Código de tarjeta terminada en 1234", None),
            ("Código de account ending in 1234", None),
            ("Código 2500 CLP", None),
            ("Código 2500$", None),
            ("Your code is 482913; expires 27-11.", Some("482913")),
            ("Your code is 48 29.", Some("4829")),
            ("Your code is 48-29.", Some("4829")),
            ("Código 2500 €", None),
            ("Código €2500", None),
            ("Código 2500 CLP para comprar", None),
            ("Código 2.500 pesos", None),
            ("Código $2500", None),
            ("Código 2500 puntos", None),
            ("Código 25.000", None),
            ("Código 12.345.678-9", None),
            ("Código 12-05-2026", None),
            ("Código 1-5-2026", None),
            ("Código 12-05-26", None),
            ("Código 1/5/2026", None),
            ("Código 12.05.26", None),
            ("Código 2026-5-1", None),
            ("Código 12/05", None),
            ("Código +56987654321", None),
            ("Código 987654321", None),
        ] {
            assert_eq!(
                detect(text).as_ref().map(|c| c.digits.as_str()),
                expected,
                "{text}"
            );
        }
    }
    #[test]
    fn masking_sanitization_preview_and_expiry() {
        let text = "Código 7731-22";
        let code = detect(text).unwrap();
        assert_eq!(masked(text, Some(&code)), "Código ••••-••");
        let repeated =
            "Código 482913 para entrar. No lo compartas con ninguna otra persona. Repito: 482913.";
        let code = detect(repeated).unwrap();
        assert_eq!(code.ranges.len(), 2);
        assert!(!masked(repeated, Some(&code)).contains("482913"));
        assert_eq!(clean("Bank\u{202e}\u{0007}\nHello"), "Bank\nHello");
        assert_eq!(
            preview(&format!("{}👩‍👩‍👧‍👦", "x".repeat(299))),
            format!("{}…", "x".repeat(299))
        );
        assert!(!fresh(1, 120_001));
        assert!(fresh(2, 120_001));
    }
}

/// Deliberately redacted even if a prepared clipboard operation is diagnosed.
#[derive(Clone)]
pub struct SecretText(pub String, pub bool);
impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretText([redacted])")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub device: pneu_proto::Id,
    pub id: pneu_proto::Id,
}
#[derive(Default)]
pub(crate) struct State {
    pub messages: std::collections::VecDeque<(Target, pneu_proto::Sms)>,
    pub bursts: std::collections::HashMap<pneu_proto::Id, Burst>,
}
pub(crate) struct Burst {
    pub started: std::time::Instant,
    pub count: usize,
    pub senders: std::collections::BTreeSet<String>,
    pub notification: u32,
}
impl State {
    pub fn remember(&mut self, target: Target, sms: pneu_proto::Sms) -> bool {
        if self.messages.iter().any(|(old, _)| old == &target) {
            return false;
        }
        if self.messages.len() >= 256 {
            self.messages.pop_front();
        }
        self.messages.push_back((target, sms));
        true
    }
}
