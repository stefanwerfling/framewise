//! Localisation.
//!
//! Source texts in the code are English and wrapped in [`tr!`] / [`trf!`].
//! Translations live in `locales/<lang>.toml` as `"English" = "Translation"`
//! and are embedded at compile time. Missing entries fall back to English.
//!
//! Placeholders are positional `{}`; `{{` and `}}` produce literal braces.
//! Callers pre-format numbers (`format!("{x:.2}")`) when precision matters.
//!
//! Domain data (warnings, detections) is stored structured and only
//! translated when displayed, so nothing language-specific is persisted.

use std::collections::HashMap;
use std::fmt::{Display, Write as _};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

const GERMAN_TOML: &str = include_str!("../locales/de.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Language {
    #[default]
    English,
    German,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::English, Language::German];

    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::German => "Deutsch",
        }
    }

    /// Guess from the environment (`LC_ALL`, `LC_MESSAGES`, `LANG`).
    pub fn from_env() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|v| !v.is_empty())
            .map_or(Language::English, |v| {
                if v.to_ascii_lowercase().starts_with("de") {
                    Language::German
                } else {
                    Language::English
                }
            })
    }
}

// The single piece of global state: the display language. It only affects
// presentation, never domain behaviour.
static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_language(lang: Language) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn language() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::German,
        _ => Language::English,
    }
}

fn parse_catalog(text: &str, name: &str) -> HashMap<String, String> {
    match toml::from_str::<HashMap<String, String>>(text) {
        Ok(map) => map,
        Err(err) => {
            tracing::error!("translation file {name} is invalid: {err}");
            HashMap::new()
        }
    }
}

fn german() -> &'static HashMap<String, String> {
    static CATALOG: OnceLock<HashMap<String, String>> = OnceLock::new();
    CATALOG.get_or_init(|| parse_catalog(GERMAN_TOML, "de.toml"))
}

/// Translate an English source text into the current language.
pub fn translate(en: &'static str) -> &'static str {
    match language() {
        Language::English => en,
        Language::German => german().get(en).map_or(en, String::as_str),
    }
}

/// Substitute positional `{}` placeholders.
pub fn format_template(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' if chars.peek() == Some(&'}') => {
                chars.next();
                if let Some(arg) = args.next() {
                    let _ = write!(out, "{arg}");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `tr!("Skip")` → `&'static str` in the current language.
#[macro_export]
macro_rules! tr {
    ($en:literal $(,)?) => {
        $crate::i18n::translate($en)
    };
}

/// `trf!("File {}", name)` → `String` in the current language.
#[macro_export]
macro_rules! trf {
    ($en:literal $(, $arg:expr)* $(,)?) => {{
        // Block scope: the `&dyn Display` temporaries must not live across
        // an `.await` of the caller (they are not `Send`).
        let text: String = $crate::i18n::format_template(
            $crate::i18n::translate($en),
            &[$(&$arg as &dyn ::std::fmt::Display),*],
        );
        text
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn formats_placeholders() {
        assert_eq!(format_template("a {} b {} c", &[&1, &"x"]), "a 1 b x c");
        assert_eq!(format_template("{{literal}} {}", &[&5]), "{literal} 5");
        assert_eq!(format_template("missing {}", &[]), "missing ");
    }

    fn placeholder_count(s: &str) -> usize {
        let cleaned = s.replace("{{", "").replace("}}", "");
        cleaned.matches("{}").count()
    }

    /// Extract the first string literal after every `tr!(` / `trf!(`.
    fn source_texts() -> Vec<(String, String)> {
        fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for marker in ["tr!(", "trf!("] {
                        for (pos, _) in text.match_indices(marker) {
                            let rest = text[pos + marker.len()..].trim_start();
                            let Some(rest) = rest.strip_prefix('"') else {
                                continue;
                            };
                            let mut lit = String::new();
                            let mut chars = rest.chars();
                            while let Some(c) = chars.next() {
                                match c {
                                    '\\' => {
                                        if let Some(n) = chars.next() {
                                            lit.push(match n {
                                                'n' => '\n',
                                                't' => '\t',
                                                other => other,
                                            });
                                        }
                                    }
                                    '"' => break,
                                    other => lit.push(other),
                                }
                            }
                            out.push((path.display().to_string(), lit));
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    #[test]
    fn german_catalog_is_complete_and_consistent() {
        let catalog: HashMap<String, String> =
            toml::from_str(GERMAN_TOML).unwrap_or_else(|e| panic!("de.toml must parse: {e}"));
        let mut missing = Vec::new();
        for (file, text) in source_texts() {
            // skip the macro definitions/docs in this file
            if text.is_empty() || file.ends_with("i18n.rs") {
                continue;
            }
            match catalog.get(&text) {
                None => missing.push(format!("{file}: {text:?}")),
                Some(de) => assert_eq!(
                    placeholder_count(&text),
                    placeholder_count(de),
                    "placeholder mismatch for {text:?}"
                ),
            }
        }
        assert!(
            missing.is_empty(),
            "missing German translations:\n{}",
            missing.join("\n")
        );
    }
}
