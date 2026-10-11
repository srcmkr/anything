//! Display formatting for names, sizes and dates in the interface language.

use std::{borrow::Cow, ops::Range, sync::OnceLock};

use chrono::{DateTime, Local, TimeZone};

use crate::i18n::{self, Lang};

/// A file name or path as shown. Control characters and those that turn
/// the direction of text around could make a name look like another one
/// (`Rechnung\u{202E}fdp.exe` shows as "Rechnungexe.pdf"), so they appear as
/// visible marks. `ranges` (highlights in `text`) move along.
pub fn shown(text: String, ranges: Vec<Range<usize>>) -> (String, Vec<Range<usize>>) {
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) || !text.chars().any(|c| mark(c).is_some()) {
        return (text, ranges);
    }
    let mut out = String::with_capacity(text.len() + 8);
    // New offset of every old one; only char boundaries are ever looked up.
    let mut moved = vec![0; text.len() + 1];
    for (at, c) in text.char_indices() {
        moved[at] = out.len();
        out.push(mark(c).unwrap_or(c));
    }
    moved[text.len()] = out.len();
    let ranges = ranges.into_iter().map(|r| moved[r.start]..moved[r.end]).collect();
    (out, ranges)
}

fn mark(c: char) -> Option<char> {
    match c {
        // Control pictures: ␀ … ␟, ␡.
        '\0'..='\x1f' => char::from_u32(0x2400 + c as u32),
        '\x7f' => Some('\u{2421}'),
        '\u{80}'..='\u{9f}'
        | '\u{61c}'
        | '\u{200e}'
        | '\u{200f}'
        | '\u{2028}'
        | '\u{2029}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}' => Some('\u{fffd}'),
        // Bytes of a name that is not valid UTF-8.
        c if anything_core::name::is_raw_byte(c) => Some('\u{fffd}'),
        _ => None,
    }
}

/// The home folder as `~`: `/home/me/Bilder` shows as `~/Bilder`.
pub fn tilde(path: &str) -> Cow<'_, str> {
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    let home = HOME.get_or_init(|| {
        std::env::var("HOME").ok().map(|h| h.trim_end_matches('/').to_owned()).filter(|h| !h.is_empty())
    });
    tilde_in(path, home.as_deref())
}

fn tilde_in<'a>(path: &'a str, home: Option<&str>) -> Cow<'a, str> {
    match home.and_then(|home| path.strip_prefix(home)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => Cow::Owned(format!("~{rest}")),
        _ => Cow::Borrowed(path),
    }
}

/// Fits a path into `max` by leaving out folders in the middle, so its start
/// and the folders nearest the end stay: `~/…/Dokumente/Steuer`. `width`
/// measures text. When not even the last part fits after the start, the
/// result is `…/last`, to be cut further from the start.
pub fn fit_path(path: &str, max: f32, width: impl Fn(&str) -> f32) -> String {
    if width(path) <= max {
        return path.to_owned();
    }
    let parts: Vec<&str> = path.split('/').collect();
    // What stays at the start: `~`, or the first folder below `/`.
    let head = if parts.first().is_some_and(|p| p.is_empty()) { 2 } else { 1 };
    let last = parts.last().copied().unwrap_or_default();
    if parts.len() <= head + 1 {
        return format!("…/{last}");
    }
    let slash = width("/");
    let start = parts[..head].join("/");
    let mut used = width(&start) + width("/…/");
    // At least one folder is left out.
    let mut keep = 0;
    for part in parts[head + 1..].iter().rev() {
        let more = width(part) + if keep > 0 { slash } else { 0.0 };
        if used + more > max {
            break;
        }
        used += more;
        keep += 1;
    }
    if keep == 0 {
        return format!("…/{last}");
    }
    format!("{start}/…/{}", parts[parts.len() - keep..].join("/"))
}

/// Human readable size like file managers show it.
pub fn size(bytes: u64) -> String {
    size_in(bytes, i18n::current())
}

fn size_in(bytes: u64, lang: Lang) -> String {
    let units: [&str; 5] = match lang {
        // French counts octets.
        Lang::Fr => ["o", "Ko", "Mo", "Go", "To"],
        Lang::Ru => ["Б", "КБ", "МБ", "ГБ", "ТБ"],
        _ => ["B", "KB", "MB", "GB", "TB"],
    };
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} {}", units[0]);
    }
    format!("{value:.1} {}", units[unit]).replace('.', lang.decimal().encode_utf8(&mut [0; 4]))
}

/// Local date and time: `2026-10-11 14:03`, `11.10.2026 14:03`, … as is usual
/// for the language.
pub fn time(secs: i64) -> String {
    time_in(secs, &Local, i18n::current())
}

fn time_in<Tz: TimeZone>(secs: i64, zone: &Tz, lang: Lang) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(utc) = DateTime::from_timestamp(secs, 0) else { return String::new() };
    let local = utc.with_timezone(zone);
    let pattern = match lang {
        Lang::En => "%Y-%m-%d %H:%M",
        Lang::De | Lang::Ru => "%d.%m.%Y %H:%M",
        Lang::Es | Lang::Fr | Lang::PtBr => "%d/%m/%Y %H:%M",
        Lang::ZhCn => "%Y/%m/%d %H:%M",
    };
    local.format(pattern).to_string()
}

#[cfg(test)]
// Highlights are lists of ranges, also when there is one.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    #[test]
    fn formats() {
        assert_eq!(time_in(0, &Utc, Lang::En), "1970-01-01 00:00");
        assert_eq!(time_in(1_760_140_800, &Utc, Lang::De), "11.10.2025 00:00");
        let berlin_summer = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(time_in(1_760_140_800, &berlin_summer, Lang::De), "11.10.2025 02:00");
        assert_eq!(size_in(512, Lang::De), "512 B");
        assert_eq!(size_in(1536, Lang::En), "1.5 KB");
        assert_eq!(size_in(1536, Lang::De), "1,5 KB");
        assert_eq!(size_in(1536, Lang::Fr), "1,5 Ko");
        assert_eq!(size_in(512, Lang::Ru), "512 Б");
        assert_eq!(size_in(1536, Lang::ZhCn), "1.5 KB");
        assert_eq!(time_in(1_760_140_800, &Utc, Lang::Fr), "11/10/2025 00:00");
        assert_eq!(time_in(1_760_140_800, &Utc, Lang::ZhCn), "2025/10/11 00:00");
    }

    #[test]
    fn shortens_paths_in_the_middle() {
        assert_eq!(tilde_in("/home/me/Bilder", Some("/home/me")), "~/Bilder");
        assert_eq!(tilde_in("/home/me", Some("/home/me")), "~");
        assert_eq!(tilde_in("/home/meyer/x", Some("/home/me")), "/home/meyer/x");
        assert_eq!(tilde_in("/srv/x", None), "/srv/x");
        // One unit per character.
        let w = |s: &str| s.chars().count() as f32;
        let path = "~/日本語/Musik/Backup/Scans 2020/Dokumente/Steuer";
        assert_eq!(fit_path(path, 100.0, w), path);
        assert_eq!(fit_path(path, 26.0, w), "~/…/Dokumente/Steuer");
        assert_eq!(fit_path(path, 19.0, w), "~/…/Steuer");
        assert_eq!(fit_path("/srv/data/2024/Belege/Steuer", 20.0, w), "/srv/…/Belege/Steuer");
        // Nothing but the end fits.
        assert_eq!(fit_path(path, 5.0, w), "…/Steuer");
        assert_eq!(fit_path("/srv/Steuer", 3.0, w), "…/Steuer");
    }

    #[test]
    fn shows_hidden_characters() {
        let (text, ranges) = shown("plain.pdf".into(), vec![0..5]);
        assert_eq!((text.as_str(), ranges), ("plain.pdf", vec![0..5]));
        // Right-to-left override: would read "Rechnungexe.pdf".
        let name = "Rechnung\u{202e}fdp.exe".to_string();
        let (text, ranges) = shown(name, vec![0..8, 11..14]);
        assert_eq!(text, "Rechnung\u{fffd}fdp.exe");
        assert_eq!(&text[ranges[1].clone()], "fdp");
        let (text, _) = shown("zwei\nZeilen\t\x7f".into(), Vec::new());
        assert_eq!(text, "zwei\u{240a}Zeilen\u{2409}\u{2421}");
        let (text, ranges) = shown("Ü\u{1}ber".into(), vec![3..6]);
        assert_eq!(&text[ranges[0].clone()], "ber");
    }
}
