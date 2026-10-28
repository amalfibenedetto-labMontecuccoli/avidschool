//! The interface language: English (the app's own) or Italian. Texts are looked up by their English
//! wording in `it.txt` (one `English => Italiano` per line); a text without a translation stays
//! English, so the interface never shows a blank.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    English,
    Italian,
}

static LANG: AtomicU8 = AtomicU8::new(0);

const ITALIAN: &str = include_str!("it.txt");

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 { Lang::Italian } else { Lang::English }
}

pub fn set_lang(lang: Lang) {
    LANG.store(u8::from(lang == Lang::Italian), Ordering::Relaxed);
}

fn table() -> &'static HashMap<&'static str, &'static str> {
    static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    TABLE.get_or_init(|| ITALIAN.lines().filter_map(|l| l.trim_end_matches('\r').split_once(" => ")).collect())
}

/// `text` in the interface language.
pub fn tr(text: &str) -> &str {
    if lang() == Lang::English {
        return text;
    }
    table().get(text).copied().unwrap_or(text)
}

/// A text that starts with a word that can be translated and goes on with a name, such as
/// "Undo Paste": the first word is translated, the rest stays as it is.
pub fn tr_prefixed(text: &str) -> String {
    if lang() == Lang::English {
        return text.to_string();
    }
    match text.split_once(' ') {
        Some((first, rest)) if matches!(first, "Undo" | "Redo") => format!("{} {rest}", tr(first)),
        _ => tr(text).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn italian_table_parses_and_english_is_untouched() {
        set_lang(Lang::Italian);
        assert_eq!(tr("Paste"), "Incolla");
        assert_eq!(tr("not in the table"), "not in the table");
        assert_eq!(tr_prefixed("Undo Paste"), "Annulla Paste");
        set_lang(Lang::English);
        assert_eq!(tr("Paste"), "Paste");
    }
}
