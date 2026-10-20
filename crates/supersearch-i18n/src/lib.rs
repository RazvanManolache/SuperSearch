//! Runtime language support: keyed UI strings with an English
//! fallback, loaded from embedded `lang/*.json` files.
//!
//! The GUI looks every label, button, hover tooltip, and status line
//! up here by key ([`I18n::t`]; [`I18n::f`] fills `{placeholders}`).
//! Adding a language means adding one row to [`LANGUAGES`] plus one
//! JSON file with the same keys as `en.json` — no other code changes,
//! and the parity test below covers the new file automatically.
//! Unknown languages and missing keys both fall back to English, so
//! partial translations stay usable.

use std::collections::HashMap;

/// One selectable UI language: ISO code, native display name, English
/// display name (a search aid, never translated), and the embedded
/// translation file.
pub struct Language {
    /// Config-file code (`en`, `pt-BR`); matched case-insensitively.
    pub code: &'static str,
    /// Native display name shown in the language picker.
    pub label: &'static str,
    /// English name, so the picker search also matches it.
    pub english: &'static str,
    /// Embedded `lang/<code>.json` translation file.
    pub json: &'static str,
}

/// Every selectable language, in picker order (English first, then
/// Romanian, then the rest in the order added; the picker is
/// searchable, so position barely matters).
///
/// Adding a language: append one row here and drop the matching
/// `lang/<code>.json` next to `en.json`. The searchable picker,
/// config persistence, and the parity test pick it up with no
/// further edits.
const LANGUAGES: &[Language] = &[
    Language {
        code: "en",
        label: "English",
        english: "English",
        json: include_str!("../lang/en.json"),
    },
    Language {
        code: "ro",
        label: "Română",
        english: "Romanian",
        json: include_str!("../lang/ro.json"),
    },
    Language {
        code: "af",
        label: "Afrikaans",
        english: "Afrikaans",
        json: include_str!("../lang/af.json"),
    },
    Language {
        code: "am",
        label: "አማርኛ",
        english: "Amharic",
        json: include_str!("../lang/am.json"),
    },
    Language {
        code: "ar",
        label: "العربية",
        english: "Arabic",
        json: include_str!("../lang/ar.json"),
    },
    Language {
        code: "as",
        label: "অসমীয়া",
        english: "Assamese",
        json: include_str!("../lang/as.json"),
    },
    Language {
        code: "az",
        label: "Azərbaycan",
        english: "Azerbaijani",
        json: include_str!("../lang/az.json"),
    },
    Language {
        code: "be",
        label: "Беларуская",
        english: "Belarusian",
        json: include_str!("../lang/be.json"),
    },
    Language {
        code: "bg",
        label: "Български",
        english: "Bulgarian",
        json: include_str!("../lang/bg.json"),
    },
    Language {
        code: "bn",
        label: "বাংলা",
        english: "Bengali",
        json: include_str!("../lang/bn.json"),
    },
    Language {
        code: "bs",
        label: "Bosanski",
        english: "Bosnian",
        json: include_str!("../lang/bs.json"),
    },
    Language {
        code: "ca",
        label: "Català",
        english: "Catalan",
        json: include_str!("../lang/ca.json"),
    },
    Language {
        code: "cs",
        label: "Čeština",
        english: "Czech",
        json: include_str!("../lang/cs.json"),
    },
    Language {
        code: "cy",
        label: "Cymraeg",
        english: "Welsh",
        json: include_str!("../lang/cy.json"),
    },
    Language {
        code: "da",
        label: "Dansk",
        english: "Danish",
        json: include_str!("../lang/da.json"),
    },
    Language {
        code: "de",
        label: "Deutsch",
        english: "German",
        json: include_str!("../lang/de.json"),
    },
    Language {
        code: "el",
        label: "Ελληνικά",
        english: "Greek",
        json: include_str!("../lang/el.json"),
    },
    Language {
        code: "es",
        label: "Español",
        english: "Spanish",
        json: include_str!("../lang/es.json"),
    },
    Language {
        code: "et",
        label: "Eesti",
        english: "Estonian",
        json: include_str!("../lang/et.json"),
    },
    Language {
        code: "eu",
        label: "Euskara",
        english: "Basque",
        json: include_str!("../lang/eu.json"),
    },
    Language {
        code: "fa",
        label: "فارسی",
        english: "Persian",
        json: include_str!("../lang/fa.json"),
    },
    Language {
        code: "fi",
        label: "Suomi",
        english: "Finnish",
        json: include_str!("../lang/fi.json"),
    },
    Language {
        code: "fr",
        label: "Français",
        english: "French",
        json: include_str!("../lang/fr.json"),
    },
    Language {
        code: "fy",
        label: "Frysk",
        english: "West Frisian",
        json: include_str!("../lang/fy.json"),
    },
    Language {
        code: "ga",
        label: "Gaeilge",
        english: "Irish",
        json: include_str!("../lang/ga.json"),
    },
    Language {
        code: "gl",
        label: "Galego",
        english: "Galician",
        json: include_str!("../lang/gl.json"),
    },
    Language {
        code: "gu",
        label: "ગુજરાતી",
        english: "Gujarati",
        json: include_str!("../lang/gu.json"),
    },
    Language {
        code: "ha",
        label: "Hausa",
        english: "Hausa",
        json: include_str!("../lang/ha.json"),
    },
    Language {
        code: "he",
        label: "עברית",
        english: "Hebrew",
        json: include_str!("../lang/he.json"),
    },
    Language {
        code: "hi",
        label: "हिन्दी",
        english: "Hindi",
        json: include_str!("../lang/hi.json"),
    },
    Language {
        code: "hr",
        label: "Hrvatski",
        english: "Croatian",
        json: include_str!("../lang/hr.json"),
    },
    Language {
        code: "hu",
        label: "Magyar",
        english: "Hungarian",
        json: include_str!("../lang/hu.json"),
    },
    Language {
        code: "hy",
        label: "Հայերեն",
        english: "Armenian",
        json: include_str!("../lang/hy.json"),
    },
    Language {
        code: "id",
        label: "Bahasa Indonesia",
        english: "Indonesian",
        json: include_str!("../lang/id.json"),
    },
    Language {
        code: "ig",
        label: "Igbo",
        english: "Igbo",
        json: include_str!("../lang/ig.json"),
    },
    Language {
        code: "is",
        label: "Íslenska",
        english: "Icelandic",
        json: include_str!("../lang/is.json"),
    },
    Language {
        code: "it",
        label: "Italiano",
        english: "Italian",
        json: include_str!("../lang/it.json"),
    },
    Language {
        code: "ja",
        label: "日本語",
        english: "Japanese",
        json: include_str!("../lang/ja.json"),
    },
    Language {
        code: "ka",
        label: "ქართული",
        english: "Georgian",
        json: include_str!("../lang/ka.json"),
    },
    Language {
        code: "kk",
        label: "Қазақша",
        english: "Kazakh",
        json: include_str!("../lang/kk.json"),
    },
    Language {
        code: "km",
        label: "ខ្មែរ",
        english: "Khmer",
        json: include_str!("../lang/km.json"),
    },
    Language {
        code: "kn",
        label: "ಕನ್ನಡ",
        english: "Kannada",
        json: include_str!("../lang/kn.json"),
    },
    Language {
        code: "ko",
        label: "한국어",
        english: "Korean",
        json: include_str!("../lang/ko.json"),
    },
    Language {
        code: "ku",
        label: "Kurdî",
        english: "Kurdish",
        json: include_str!("../lang/ku.json"),
    },
    Language {
        code: "ky",
        label: "Кыргызча",
        english: "Kyrgyz",
        json: include_str!("../lang/ky.json"),
    },
    Language {
        code: "lb",
        label: "Lëtzebuergesch",
        english: "Luxembourgish",
        json: include_str!("../lang/lb.json"),
    },
    Language {
        code: "lo",
        label: "ລາວ",
        english: "Lao",
        json: include_str!("../lang/lo.json"),
    },
    Language {
        code: "lt",
        label: "Lietuvių",
        english: "Lithuanian",
        json: include_str!("../lang/lt.json"),
    },
    Language {
        code: "lv",
        label: "Latviešu",
        english: "Latvian",
        json: include_str!("../lang/lv.json"),
    },
    Language {
        code: "mk",
        label: "Македонски",
        english: "Macedonian",
        json: include_str!("../lang/mk.json"),
    },
    Language {
        code: "ml",
        label: "മലയാളം",
        english: "Malayalam",
        json: include_str!("../lang/ml.json"),
    },
    Language {
        code: "mn",
        label: "Монгол",
        english: "Mongolian",
        json: include_str!("../lang/mn.json"),
    },
    Language {
        code: "mr",
        label: "मराठी",
        english: "Marathi",
        json: include_str!("../lang/mr.json"),
    },
    Language {
        code: "ms",
        label: "Bahasa Melayu",
        english: "Malay",
        json: include_str!("../lang/ms.json"),
    },
    Language {
        code: "mt",
        label: "Malti",
        english: "Maltese",
        json: include_str!("../lang/mt.json"),
    },
    Language {
        code: "my",
        label: "မြန်မာ",
        english: "Burmese",
        json: include_str!("../lang/my.json"),
    },
    Language {
        code: "ne",
        label: "नेपाली",
        english: "Nepali",
        json: include_str!("../lang/ne.json"),
    },
    Language {
        code: "nl",
        label: "Nederlands",
        english: "Dutch",
        json: include_str!("../lang/nl.json"),
    },
    Language {
        code: "no",
        label: "Norsk",
        english: "Norwegian",
        json: include_str!("../lang/no.json"),
    },
    Language {
        code: "or",
        label: "ଓଡ଼ିଆ",
        english: "Odia",
        json: include_str!("../lang/or.json"),
    },
    Language {
        code: "pa",
        label: "ਪੰਜਾਬੀ",
        english: "Punjabi",
        json: include_str!("../lang/pa.json"),
    },
    Language {
        code: "pl",
        label: "Polski",
        english: "Polish",
        json: include_str!("../lang/pl.json"),
    },
    Language {
        code: "ps",
        label: "پښتو",
        english: "Pashto",
        json: include_str!("../lang/ps.json"),
    },
    Language {
        code: "pt",
        label: "Português",
        english: "Portuguese",
        json: include_str!("../lang/pt.json"),
    },
    Language {
        code: "pt-BR",
        label: "Português (Brasil)",
        english: "Portuguese (Brazil)",
        json: include_str!("../lang/pt-BR.json"),
    },
    Language {
        code: "qu",
        label: "Runa simi",
        english: "Quechua",
        json: include_str!("../lang/qu.json"),
    },
    Language {
        code: "ru",
        label: "Русский",
        english: "Russian",
        json: include_str!("../lang/ru.json"),
    },
    Language {
        code: "si",
        label: "සිංහල",
        english: "Sinhala",
        json: include_str!("../lang/si.json"),
    },
    Language {
        code: "sk",
        label: "Slovenčina",
        english: "Slovak",
        json: include_str!("../lang/sk.json"),
    },
    Language {
        code: "sl",
        label: "Slovenščina",
        english: "Slovenian",
        json: include_str!("../lang/sl.json"),
    },
    Language {
        code: "so",
        label: "Soomaali",
        english: "Somali",
        json: include_str!("../lang/so.json"),
    },
    Language {
        code: "sq",
        label: "Shqip",
        english: "Albanian",
        json: include_str!("../lang/sq.json"),
    },
    Language {
        code: "sr",
        label: "Српски",
        english: "Serbian",
        json: include_str!("../lang/sr.json"),
    },
    Language {
        code: "sv",
        label: "Svenska",
        english: "Swedish",
        json: include_str!("../lang/sv.json"),
    },
    Language {
        code: "sw",
        label: "Kiswahili",
        english: "Swahili",
        json: include_str!("../lang/sw.json"),
    },
    Language {
        code: "ta",
        label: "தமிழ்",
        english: "Tamil",
        json: include_str!("../lang/ta.json"),
    },
    Language {
        code: "te",
        label: "తెలుగు",
        english: "Telugu",
        json: include_str!("../lang/te.json"),
    },
    Language {
        code: "th",
        label: "ไทย",
        english: "Thai",
        json: include_str!("../lang/th.json"),
    },
    Language {
        code: "ti",
        label: "ትግርኛ",
        english: "Tigrinya",
        json: include_str!("../lang/ti.json"),
    },
    Language {
        code: "tk",
        label: "Türkmençe",
        english: "Turkmen",
        json: include_str!("../lang/tk.json"),
    },
    Language {
        code: "tl",
        label: "Filipino",
        english: "Filipino",
        json: include_str!("../lang/tl.json"),
    },
    Language {
        code: "tr",
        label: "Türkçe",
        english: "Turkish",
        json: include_str!("../lang/tr.json"),
    },
    Language {
        code: "uk",
        label: "Українська",
        english: "Ukrainian",
        json: include_str!("../lang/uk.json"),
    },
    Language {
        code: "ur",
        label: "اردو",
        english: "Urdu",
        json: include_str!("../lang/ur.json"),
    },
    Language {
        code: "uz",
        label: "Oʻzbek",
        english: "Uzbek",
        json: include_str!("../lang/uz.json"),
    },
    Language {
        code: "vi",
        label: "Tiếng Việt",
        english: "Vietnamese",
        json: include_str!("../lang/vi.json"),
    },
    Language {
        code: "xh",
        label: "isiXhosa",
        english: "Xhosa",
        json: include_str!("../lang/xh.json"),
    },
    Language {
        code: "yo",
        label: "Yorùbá",
        english: "Yoruba",
        json: include_str!("../lang/yo.json"),
    },
    Language {
        code: "zh",
        label: "中文",
        english: "Chinese (Simplified)",
        json: include_str!("../lang/zh.json"),
    },
    Language {
        code: "zh-TW",
        label: "中文（繁體）",
        english: "Chinese (Traditional)",
        json: include_str!("../lang/zh-TW.json"),
    },
    Language {
        code: "zu",
        label: "isiZulu",
        english: "Zulu",
        json: include_str!("../lang/zu.json"),
    },
    Language {
        code: "jv",
        label: "Basa Jawa",
        english: "Javanese",
        json: include_str!("../lang/jv.json"),
    },
    Language {
        code: "su",
        label: "Basa Sunda",
        english: "Sundanese",
        json: include_str!("../lang/su.json"),
    },
    Language {
        code: "yue",
        label: "粵語",
        english: "Cantonese",
        json: include_str!("../lang/yue.json"),
    },
    Language {
        code: "bho",
        label: "भोजपुरी",
        english: "Bhojpuri",
        json: include_str!("../lang/bho.json"),
    },
    Language {
        code: "mai",
        label: "मैथिली",
        english: "Maithili",
        json: include_str!("../lang/mai.json"),
    },
    Language {
        code: "om",
        label: "Afaan Oromoo",
        english: "Oromo",
        json: include_str!("../lang/om.json"),
    },
    Language {
        code: "ff",
        label: "Fulfulde",
        english: "Fulah",
        json: include_str!("../lang/ff.json"),
    },
    Language {
        code: "sd",
        label: "سنڌي",
        english: "Sindhi",
        json: include_str!("../lang/sd.json"),
    },
    Language {
        code: "mg",
        label: "Malagasy",
        english: "Malagasy",
        json: include_str!("../lang/mg.json"),
    },
    Language {
        code: "ceb",
        label: "Bisaya",
        english: "Cebuano",
        json: include_str!("../lang/ceb.json"),
    },
    Language {
        code: "skr",
        label: "سرائیکی",
        english: "Saraiki",
        json: include_str!("../lang/skr.json"),
    },
    Language {
        code: "ln",
        label: "Lingála",
        english: "Lingala",
        json: include_str!("../lang/ln.json"),
    },
    Language {
        code: "ug",
        label: "ئۇيغۇرچە",
        english: "Uyghur",
        json: include_str!("../lang/ug.json"),
    },
    Language {
        code: "sn",
        label: "chiShona",
        english: "Shona",
        json: include_str!("../lang/sn.json"),
    },
    Language {
        code: "rw",
        label: "Ikinyarwanda",
        english: "Kinyarwanda",
        json: include_str!("../lang/rw.json"),
    },
    Language {
        code: "ny",
        label: "Chichewa",
        english: "Chichewa",
        json: include_str!("../lang/ny.json"),
    },
    Language {
        code: "ak",
        label: "Akan",
        english: "Akan",
        json: include_str!("../lang/ak.json"),
    },
    Language {
        code: "wo",
        label: "Wolof",
        english: "Wolof",
        json: include_str!("../lang/wo.json"),
    },
    Language {
        code: "za",
        label: "Vahcuengh",
        english: "Zhuang",
        json: include_str!("../lang/za.json"),
    },
    Language {
        code: "ks",
        label: "کٲشُر",
        english: "Kashmiri",
        json: include_str!("../lang/ks.json"),
    },
    Language {
        code: "kok",
        label: "कोंकणी",
        english: "Konkani",
        json: include_str!("../lang/kok.json"),
    },
    Language {
        code: "sat",
        label: "ᱥᱟᱱᱛᱟᱲᱤ",
        english: "Santali",
        json: include_str!("../lang/sat.json"),
    },
    Language {
        code: "bo",
        label: "བོད་སྐད་",
        english: "Tibetan",
        json: include_str!("../lang/bo.json"),
    },
    Language {
        code: "rm",
        label: "Rumantsch",
        english: "Romansh",
        json: include_str!("../lang/rm.json"),
    },
    Language {
        code: "mi",
        label: "Te reo Māori",
        english: "Māori",
        json: include_str!("../lang/mi.json"),
    },
    Language {
        code: "haw",
        label: "ʻŌlelo Hawaiʻi",
        english: "Hawaiian",
        json: include_str!("../lang/haw.json"),
    },
    Language {
        code: "sm",
        label: "Gagana Samoa",
        english: "Samoan",
        json: include_str!("../lang/sm.json"),
    },
    Language {
        code: "kl",
        label: "Kalaallisut",
        english: "Greenlandic",
        json: include_str!("../lang/kl.json"),
    },
    Language {
        code: "dz",
        label: "རྫོང་ཁ",
        english: "Dzongkha",
        json: include_str!("../lang/dz.json"),
    },
    Language {
        code: "dv",
        label: "ދިވެހި",
        english: "Divehi",
        json: include_str!("../lang/dv.json"),
    },
    Language {
        code: "tg",
        label: "Тоҷикӣ",
        english: "Tajik",
        json: include_str!("../lang/tg.json"),
    },
    Language {
        code: "tt",
        label: "Татарча",
        english: "Tatar",
        json: include_str!("../lang/tt.json"),
    },
    Language {
        code: "ht",
        label: "Kreyòl ayisyen",
        english: "Haitian Creole",
        json: include_str!("../lang/ht.json"),
    },
    Language {
        code: "gn",
        label: "Avañe'ẽ",
        english: "Guarani",
        json: include_str!("../lang/gn.json"),
    },
    Language {
        code: "ay",
        label: "Aymar aru",
        english: "Aymara",
        json: include_str!("../lang/ay.json"),
    },
    Language {
        code: "ckb",
        label: "کوردیی ناوەندی",
        english: "Sorani Kurdish",
        json: include_str!("../lang/ckb.json"),
    },
    Language {
        code: "eo",
        label: "Esperanto",
        english: "Esperanto",
        json: include_str!("../lang/eo.json"),
    },
    Language {
        code: "la",
        label: "Latina",
        english: "Latin",
        json: include_str!("../lang/la.json"),
    },
    Language {
        code: "yi",
        label: "ייִדיש",
        english: "Yiddish",
        json: include_str!("../lang/yi.json"),
    },
    Language {
        code: "sa",
        label: "संस्कृतम्",
        english: "Sanskrit",
        json: include_str!("../lang/sa.json"),
    },
];

/// Selectable UI language: an index into [`LANGUAGES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lang {
    idx: usize,
}

impl Lang {
    /// English (also the fallback for every other language).
    pub const En: Lang = Lang { idx: 0 };
    /// Română.
    pub const Ro: Lang = Lang { idx: 1 };

    /// All selectable languages, in picker order.
    pub fn all() -> Vec<Lang> {
        (0..LANGUAGES.len()).map(|idx| Lang { idx }).collect()
    }

    /// Config-file code (`en`, `pt-BR`). Matched case-insensitively.
    pub fn code(self) -> &'static str {
        LANGUAGES[self.idx].code
    }

    /// Native display name for the language picker.
    pub fn label(self) -> &'static str {
        LANGUAGES[self.idx].label
    }

    /// English display name (picker search aid, never translated).
    pub fn english(self) -> &'static str {
        LANGUAGES[self.idx].english
    }

    /// Parse a config-file code; anything unknown means English.
    pub fn from_code(s: &str) -> Self {
        let want = s.trim().to_ascii_lowercase();
        LANGUAGES
            .iter()
            .position(|l| l.code.to_ascii_lowercase() == want)
            .map(|idx| Lang { idx })
            .unwrap_or(Lang::En)
    }
}

/// Keyed strings for one language, with an English fallback map.
#[derive(Debug, Clone)]
pub struct I18n {
    lang: Lang,
    map: HashMap<String, String>,
    fallback: HashMap<String, String>,
}

impl I18n {
    /// Load `lang` (embedded JSON). Missing keys and unknown
    /// languages fall back to English; the English file itself is
    /// embedded, so construction never fails at runtime.
    pub fn load(lang: Lang) -> Self {
        let fallback: HashMap<String, String> =
            serde_json::from_str(LANGUAGES[0].json).expect("embedded en.json parses");
        let map = if lang == Lang::En {
            fallback.clone()
        } else {
            let own: HashMap<String, String> = serde_json::from_str(LANGUAGES[lang.idx].json)
                .unwrap_or_else(|e| panic!("embedded lang/{}.json parses: {e}", lang.code()));
            let mut merged = fallback.clone();
            merged.extend(own);
            merged
        };
        Self {
            lang,
            map,
            fallback,
        }
    }

    /// Active language.
    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// Owned English string for `key`; unknown keys yield the key
    /// itself so a missing entry is visible instead of blank. Owned
    /// on purpose: callers hold `&mut` borrows (often through `Deref`)
    /// in the same expression, so no borrow may cross the call.
    pub fn t(&self, key: &str) -> String {
        self.map
            .get(key)
            .or_else(|| self.fallback.get(key))
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    /// [`I18n::t`] with `{placeholder}` substitution from `args`.
    pub fn f(&self, key: &str, args: &[(&str, &str)]) -> String {
        fill(&self.t(key), args)
    }
}

/// Substitute `{placeholder}` pairs into a template. Unknown
/// placeholders are left in place; this is also what worker threads
/// use (they own the template `String`, so no borrow crosses the
/// thread boundary).
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit-test seam: an `I18n` with a deliberately partial map,
    /// so the fallback chain is exercisable without a real partial
    /// file on disk.
    fn partial(lang: Lang, pairs: &[(&str, &str)]) -> I18n {
        let fallback: HashMap<String, String> =
            serde_json::from_str(include_str!("../lang/en.json")).expect("en.json parses");
        I18n {
            lang,
            map: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            fallback,
        }
    }

    #[test]
    fn missing_keys_fall_back_to_english() {
        let tr = partial(Lang::Ro, &[("only.ro", "doar RO")]);
        // Present in the language map: translated value wins.
        assert_eq!(tr.t("only.ro"), "doar RO");
        // Absent from the language map: English fallback, not blank.
        assert_eq!(tr.t("run.scan"), I18n::load(Lang::En).t("run.scan"));
        // Absent everywhere: the key itself echoes (visible, not blank).
        assert_eq!(tr.t("no.such.key"), "no.such.key");
        // Templates fall back the same way, then fill normally.
        assert_eq!(
            tr.f(
                "preview.meta",
                &[("size", "1"), ("full", "2"), ("kind", "3")]
            ),
            I18n::load(Lang::En).f(
                "preview.meta",
                &[("size", "1"), ("full", "2"), ("kind", "3")]
            )
        );
    }

    #[test]
    fn all_languages_load_without_panicking() {
        // Extend automatically: every `Lang::all` variant must load
        // and serve a non-empty string for a core key.
        for lang in Lang::all() {
            let tr = I18n::load(lang);
            assert_eq!(tr.lang(), lang);
            assert!(!tr.t("run.scan").is_empty());
        }
    }

    #[test]
    fn translation_files_stay_in_parity() {
        // Covered automatically: every non-English row of
        // `LANGUAGES` must carry the full English key set.
        let en: HashMap<String, String> =
            serde_json::from_str(LANGUAGES[0].json).expect("en.json parses");
        for lang in LANGUAGES.iter().skip(1) {
            let map: HashMap<String, String> = serde_json::from_str(lang.json)
                .unwrap_or_else(|e| panic!("{}.json parses: {e}", lang.code));
            let mut missing: Vec<&String> = en.keys().filter(|k| !map.contains_key(*k)).collect();
            missing.sort();
            assert!(
                missing.is_empty(),
                "{}.json lacks keys: {missing:?}",
                lang.code
            );
        }
    }

    #[test]
    fn unknown_language_and_key_fall_back_to_english() {
        let en = I18n::load(Lang::En);
        assert_eq!(en.lang(), Lang::En);
        assert_eq!(Lang::from_code("ro"), Lang::Ro);
        assert_eq!(Lang::from_code("xx"), Lang::En);
        assert_eq!(Lang::from_code("PT-br"), Lang::from_code("pt-BR"));
        // Every embedded file parses and the fallback is non-trivial.
        assert!(en.map.len() > 100);
        // Unknown keys echo instead of blanking.
        assert_eq!(en.t("no.such.key"), "no.such.key");
    }

    #[test]
    fn templates_fill_placeholders() {
        assert_eq!(
            fill("{n} hits ({m} scanned)", &[("n", "3"), ("m", "10")]),
            "3 hits (10 scanned)"
        );
        // Unknown placeholders survive (visible, not blank).
        assert_eq!(fill("{x}!", &[]), "{x}!");
    }

    #[test]
    fn romanian_overrides_english() {
        let ro = I18n::load(Lang::Ro);
        assert_eq!(ro.lang(), Lang::Ro);
        // A translated key differs; an untranslated one falls back.
        assert_ne!(ro.t("run.scan"), I18n::load(Lang::En).t("run.scan"));
    }
}
