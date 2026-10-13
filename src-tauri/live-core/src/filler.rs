//! Filler-word filter ("ừm", "à", "oh", "uh"…) for displayed captions.
//!
//! Applied at render time to whole caption strings, so a filler whose letters
//! arrive across several streaming deltas is still recognised once complete.
//! Only standalone words are removed (never parts of words); stretched forms
//! ("ừmmm", "uhhh") match their base form; a sentence-final mark on a removed
//! word moves to the previous word ("Vậy à?" → "Vậy?"); and when a filler
//! started a sentence the next word is capitalised ("Ừm, chúng ta" → "Chúng ta").
//!
//! The same algorithm exists in `src/js/filler-filter.js` for the main window.

use std::collections::HashSet;

/// Default list (single words; multi-word phrases are not matched). Deliberately excludes words that carry meaning in Vietnamese
/// ("ừ" = yes, "ạ" = politeness particle).
pub const DEFAULT_FILLERS: &[&str] = &[
    "ừm", "ờm", "ưm", "hừm", "ờ", "à", "um", "uhm", "uh", "er", "erm", "ah", "oh", "hmm",
];

fn is_edge_punct(c: char) -> bool {
    !c.is_alphanumeric()
}

/// Lowercase, strip surrounding punctuation, collapse repeated letters.
pub fn normalize_token(w: &str) -> String {
    let core = w.trim_matches(is_edge_punct).to_lowercase();
    let mut out = String::with_capacity(core.len());
    let mut prev = None;
    for c in core.chars() {
        if Some(c) != prev {
            out.push(c);
        }
        prev = Some(c);
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct FillerFilter {
    words: HashSet<String>,
}

impl FillerFilter {
    pub fn new<S: AsRef<str>>(words: &[S]) -> Self {
        Self {
            words: words
                .iter()
                .map(|w| normalize_token(w.as_ref()))
                .filter(|w| !w.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn is_filler(&self, word: &str) -> bool {
        let n = normalize_token(word);
        !n.is_empty() && self.words.contains(&n)
    }

    /// Remove filler words from `text`, keeping leading/trailing whitespace.
    pub fn apply(&self, text: &str) -> String {
        if self.words.is_empty() || text.trim().is_empty() {
            return text.to_string();
        }
        let lead = &text[..text.len() - text.trim_start().len()];
        let trail = &text[text.trim_end().len()..];
        let mut out: Vec<String> = Vec::new();
        let mut capitalize_next = false;
        let mut removed_any = false;
        for w in text.split_whitespace() {
            if self.is_filler(w) {
                removed_any = true;
                let trailing: String = w.chars().rev().take_while(|c| is_edge_punct(*c)).collect::<Vec<_>>().into_iter().rev().collect();
                let terminal: String = trailing.chars().filter(|c| matches!(c, '.' | '?' | '!' | '…')).collect();
                if !terminal.is_empty() {
                    if let Some(prev) = out.last_mut() {
                        if matches!(prev.chars().last(), Some(',' | ';' | ':')) {
                            prev.pop();
                        }
                        if prev.chars().last().map(|c| c.is_alphanumeric()).unwrap_or(false) {
                            prev.push_str(&terminal);
                        }
                    }
                }
                if w.chars().find(|c| c.is_alphabetic()).map(|c| c.is_uppercase()).unwrap_or(false) {
                    capitalize_next = true;
                }
                continue;
            }
            if capitalize_next {
                capitalize_next = false;
                out.push(capitalize_first(w));
            } else {
                out.push(w.to_string());
            }
        }
        if !removed_any {
            return text.to_string();
        }
        if out.is_empty() {
            return String::new();
        }
        format!("{lead}{}{trail}", out.join(" "))
    }
}

fn capitalize_first(w: &str) -> String {
    let mut done = false;
    w.chars()
        .flat_map(|c| {
            if !done && c.is_alphabetic() {
                done = true;
                c.to_uppercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> FillerFilter {
        FillerFilter::new(DEFAULT_FILLERS)
    }

    #[test]
    fn removes_vietnamese_and_english_fillers() {
        assert_eq!(f().apply("Ừm, chúng ta bắt đầu nhé"), "Chúng ta bắt đầu nhé");
        assert_eq!(f().apply("Thank you, uh, everyone"), "Thank you, everyone");
        assert_eq!(f().apply("Oh, welcome to the show."), "Welcome to the show.");
        assert_eq!(f().apply("Hôm nay à, chúng ta"), "Hôm nay chúng ta");
    }

    #[test]
    fn stretched_forms_and_repeats() {
        assert_eq!(f().apply("Ừmmm ừm, ờ, xin chào"), "Xin chào");
        assert_eq!(f().apply("so ummm we start"), "so we start");
        assert_eq!(f().apply("hmmm okay"), "okay");
    }

    #[test]
    fn moves_sentence_end_to_previous_word() {
        assert_eq!(f().apply("Vậy à? Tốt quá"), "Vậy? Tốt quá");
        assert_eq!(f().apply("That is it, uh."), "That is it.");
    }

    #[test]
    fn keeps_words_that_only_contain_fillers() {
        assert_eq!(f().apply("Ohio and Erik are here"), "Ohio and Erik are here");
        assert_eq!(f().apply("ừ, được rồi ạ"), "ừ, được rồi ạ");
        assert_eq!(f().apply("cái màn hình"), "cái màn hình");
    }

    #[test]
    fn whitespace_and_empty_results() {
        assert_eq!(f().apply(" uh hello "), " hello ");
        assert_eq!(f().apply("ừm..."), "");
        assert_eq!(f().apply("   "), "   ");
        assert_eq!(FillerFilter::new::<&str>(&[]).apply("um ok"), "um ok");
    }

    #[test]
    fn custom_list() {
        let c = FillerFilter::new(&["nhé"]);
        assert_eq!(c.apply("đi thôi nhé"), "đi thôi");
    }
}
