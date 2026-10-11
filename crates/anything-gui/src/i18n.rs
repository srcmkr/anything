//! Interface language, following the desktop unless the settings say
//! otherwise. Texts live in `locales/app.yml`.

use gpui::{App, Global};

use crate::settings::{self, Language};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
    Es,
    Fr,
    PtBr,
    Ru,
    ZhCn,
}

impl Lang {
    pub const ALL: [Lang; 7] = [Lang::En, Lang::De, Lang::Es, Lang::Fr, Lang::PtBr, Lang::Ru, Lang::ZhCn];

    fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
            Lang::Es => "es",
            Lang::Fr => "fr",
            Lang::PtBr => "pt-BR",
            Lang::Ru => "ru",
            Lang::ZhCn => "zh-CN",
        }
    }

    /// The decimal separator: `1.5` or `1,5`.
    pub fn decimal(self) -> char {
        match self {
            Lang::En | Lang::ZhCn => '.',
            _ => ',',
        }
    }
}

/// The desktop's language, if we have it; English otherwise.
pub fn system() -> Lang {
    sys_locale::get_locale().map_or(Lang::En, |locale| from_locale(&locale))
}

/// `de-AT`, `pt_BR.UTF-8`, `zh-Hans-CN`, … Traditional Chinese stays English:
/// simplified characters would be the wrong script there.
fn from_locale(locale: &str) -> Lang {
    let locale = locale.to_ascii_lowercase().replace('_', "-");
    let language = locale.split(['-', '.', '@']).next().unwrap_or_default();
    match language {
        "de" => Lang::De,
        "es" => Lang::Es,
        "fr" => Lang::Fr,
        "pt" => Lang::PtBr,
        "ru" => Lang::Ru,
        "zh" if !["-tw", "-hk", "-mo", "-hant"].iter().any(|t| locale.contains(t)) => Lang::ZhCn,
        _ => Lang::En,
    }
}

pub fn current() -> Lang {
    let code = rust_i18n::locale();
    Lang::ALL.into_iter().find(|lang| lang.code() == &*code).unwrap_or(Lang::En)
}

/// Lets the tray menu follow a language change.
pub struct TrayRefresh(pub Box<dyn Fn()>);

impl Global for TrayRefresh {}

/// Switches to the language from the settings.
pub fn apply(cx: &mut App) {
    let lang = match settings::get(cx).language {
        Language::System => system(),
        Language::English => Lang::En,
        Language::German => Lang::De,
        Language::Spanish => Lang::Es,
        Language::French => Lang::Fr,
        Language::Portuguese => Lang::PtBr,
        Language::Russian => Lang::Ru,
        Language::Chinese => Lang::ZhCn,
    };
    if current() == lang && cx.has_global::<Applied>() {
        return;
    }
    cx.set_global(Applied);
    rust_i18n::set_locale(lang.code());
    gpui_component::set_locale(lang.code());
    if let Some(refresh) = cx.try_global::<TrayRefresh>() {
        (refresh.0)();
    }
    cx.refresh_windows();
}

struct Applied;

impl Global for Applied {}

/// `4999999` as `4,999,999`, `4.999.999` or `4 999 999`, as the Unicode
/// locale data (CLDR) has it.
pub fn number(n: usize, lang: Lang) -> String {
    let digits = n.to_string();
    let separator = match lang {
        Lang::En | Lang::ZhCn => ',',
        Lang::De | Lang::Es | Lang::PtBr => '.',
        // Narrow and normal no-break space.
        Lang::Fr => '\u{202f}',
        Lang::Ru => '\u{a0}',
    };
    // Spanish groups only from five digits on: 1000, but 10.000.
    if lang == Lang::Es && digits.len() <= 4 {
        return digits;
    }
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number(0, Lang::En), "0");
        assert_eq!(number(999, Lang::De), "999");
        assert_eq!(number(1000, Lang::En), "1,000");
        assert_eq!(number(4_999_999, Lang::De), "4.999.999");
        assert_eq!(number(1000, Lang::Es), "1000");
        assert_eq!(number(10_000, Lang::Es), "10.000");
        assert_eq!(number(1_234_567, Lang::Fr), "1\u{202f}234\u{202f}567");
        assert_eq!(number(1_234_567, Lang::Ru), "1\u{a0}234\u{a0}567");
        assert_eq!(number(1_234_567, Lang::ZhCn), "1,234,567");
    }

    #[test]
    fn component_texts_in_every_language() {
        let backend = crate::_rust_i18n_backend();
        for lang in Lang::ALL {
            let text = backend.translate(lang.code(), "ui.Settings.search_placeholder");
            assert!(text.is_some_and(|t| !t.is_empty()), "{lang:?}");
        }
    }

    #[test]
    fn desktop_languages() {
        assert_eq!(from_locale("de-AT"), Lang::De);
        assert_eq!(from_locale("pt_BR.UTF-8"), Lang::PtBr);
        assert_eq!(from_locale("pt-PT"), Lang::PtBr);
        assert_eq!(from_locale("es-MX"), Lang::Es);
        assert_eq!(from_locale("fr-CA"), Lang::Fr);
        assert_eq!(from_locale("ru-RU"), Lang::Ru);
        assert_eq!(from_locale("zh-CN"), Lang::ZhCn);
        assert_eq!(from_locale("zh-Hans-SG"), Lang::ZhCn);
        assert_eq!(from_locale("zh-TW"), Lang::En);
        assert_eq!(from_locale("zh-Hant-HK"), Lang::En);
        assert_eq!(from_locale("nl-NL"), Lang::En);
        assert_eq!(from_locale("C"), Lang::En);
    }
}
