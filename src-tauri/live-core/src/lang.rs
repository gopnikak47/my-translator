//! English / Vietnamese spoken-language detection from source transcript text.
//!
//! The OpenAI realtime translation API exposes no language-detection event, so
//! the bilingual router infers the spoken language from the optional source
//! transcript (`session.input_transcript.delta`). Vietnamese is written in a
//! Latin script with a dense set of diacritics (ă â đ ê ô ơ ư + five tone marks):
//! in normal Vietnamese text most syllables carry one, while English text has
//! essentially none. A sliding window over recent words with hysteresis gives a
//! cheap, deterministic and offline detector — no extra API calls or cost.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    En,
    Vi,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Vi => "vi",
        }
    }

    pub fn opposite(self) -> Lang {
        match self {
            Lang::En => Lang::Vi,
            Lang::Vi => Lang::En,
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        let c = code.trim().to_ascii_lowercase();
        // Accept BCP-47 style tags ("en-US", "vi_VN").
        match c.get(..2) {
            Some("en") => Some(Lang::En),
            Some("vi") => Some(Lang::Vi),
            _ => None,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Lang::En => 0,
            Lang::Vi => 1,
        }
    }

    pub const ALL: [Lang; 2] = [Lang::En, Lang::Vi];
}

/// Characters that occur in Vietnamese orthography but (practically) never in
/// English text. Plain `é`/`è` are excluded on purpose (café, fiancé).
const VI_CHARS: &str = "àáảãạăằắẳẵặâầấẩẫậđẻẽẹêềếểễệìíỉĩịòóỏõọôồốổỗộơờớởỡợùúủũụưừứửữựỳýỷỹỵ\
ÀÁẢÃẠĂẰẮẲẴẶÂẦẤẨẪẬĐẺẼẸÊỀẾỂỄỆÌÍỈĨỊÒÓỎÕỌÔỒỐỔỖỘƠỜỚỞỠỢÙÚỦŨỤƯỪỨỬỮỰỲÝỶỸỴ";

pub fn is_vi_char(c: char) -> bool {
    !c.is_ascii() && VI_CHARS.contains(c)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordClass {
    /// Contains a Vietnamese-only character.
    Vi,
    /// Plain ASCII letters: English, or an unaccented Vietnamese syllable.
    Latin,
}

/// Classify one whitespace-delimited token. Returns `None` for tokens with no
/// letters (numbers, punctuation).
pub fn classify_word(word: &str) -> Option<WordClass> {
    let mut letters = 0;
    let mut vi = false;
    for c in word.chars() {
        if c.is_alphabetic() {
            letters += 1;
            if is_vi_char(c) {
                vi = true;
            }
        }
    }
    if letters == 0 {
        None
    } else if vi {
        Some(WordClass::Vi)
    } else {
        Some(WordClass::Latin)
    }
}

/// Strong per-fragment hint used to route a single transcript delta before the
/// windowed detector has flipped: two or more Vietnamese-marked words.
pub fn strong_hint(text: &str) -> Option<Lang> {
    let mut vi = 0;
    let mut latin = 0;
    for w in text.split_whitespace() {
        match classify_word(w) {
            Some(WordClass::Vi) => vi += 1,
            Some(WordClass::Latin) => latin += 1,
            None => {}
        }
    }
    if vi >= 2 || (vi >= 1 && latin == 0) {
        Some(Lang::Vi)
    } else if latin >= 4 && vi == 0 {
        Some(Lang::En)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Number of recent words considered.
    pub window_words: usize,
    /// Minimum words in the window before any decision.
    pub min_words: usize,
    /// Fraction of Vietnamese-marked words at/above which the language is Vietnamese.
    pub vi_ratio: f32,
    /// Fraction at/below which the language is English.
    pub en_ratio: f32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            window_words: 8,
            min_words: 3,
            vi_ratio: 0.34,
            en_ratio: 0.10,
        }
    }
}

/// Streaming detector. Feed append-only transcript deltas; it reassembles words
/// that are split across deltas ("hel" + "lo") before classifying them.
#[derive(Debug, Clone)]
pub struct LangDetector {
    cfg: DetectorConfig,
    window: VecDeque<WordClass>,
    partial_word: String,
    current: Option<Lang>,
}

impl LangDetector {
    pub fn new(cfg: DetectorConfig) -> Self {
        Self {
            cfg,
            window: VecDeque::new(),
            partial_word: String::new(),
            current: None,
        }
    }

    pub fn current(&self) -> Option<Lang> {
        self.current
    }

    /// Seed the detector (e.g. with the configured default language) so a
    /// decision exists before the first words arrive.
    pub fn set_current(&mut self, lang: Option<Lang>) {
        self.current = lang;
    }

    /// Forget the word window (call at a long pause / new utterance so a new
    /// speaker's language is picked up after `min_words` words).
    pub fn reset_window(&mut self) {
        self.window.clear();
        self.partial_word.clear();
    }

    /// Feed a delta. Returns `Some(lang)` when the detected language changes.
    pub fn push(&mut self, delta: &str) -> Option<Lang> {
        let mut words: Vec<String> = Vec::new();
        for c in delta.chars() {
            if c.is_whitespace() {
                if !self.partial_word.is_empty() {
                    words.push(std::mem::take(&mut self.partial_word));
                }
            } else {
                self.partial_word.push(c);
            }
        }
        // A trailing partial word that already carries a Vietnamese mark can be
        // classified immediately — the mark will not disappear.
        if self.partial_word.chars().any(is_vi_char) {
            words.push(std::mem::take(&mut self.partial_word));
        }
        for w in words {
            if let Some(class) = classify_word(&w) {
                self.window.push_back(class);
                while self.window.len() > self.cfg.window_words {
                    self.window.pop_front();
                }
            }
        }
        self.decide()
    }

    fn decide(&mut self) -> Option<Lang> {
        let n = self.window.len();
        if n == 0 {
            return None;
        }
        let vi = self.window.iter().filter(|c| **c == WordClass::Vi).count();
        // Fast path: two consecutive Vietnamese-marked words is decisive even
        // before min_words is reached.
        let last_two_vi = n >= 2
            && self.window[n - 1] == WordClass::Vi
            && self.window[n - 2] == WordClass::Vi;
        let ratio = vi as f32 / n as f32;
        let decision = if last_two_vi || (n >= self.cfg.min_words && ratio >= self.cfg.vi_ratio) {
            Some(Lang::Vi)
        } else if n >= self.cfg.min_words.max(4) && ratio <= self.cfg.en_ratio {
            Some(Lang::En)
        } else {
            None
        };
        match decision {
            Some(l) if Some(l) != self.current => {
                self.current = Some(l);
                Some(l)
            }
            _ => None,
        }
    }
}

impl Default for LangDetector {
    fn default() -> Self {
        Self::new(DetectorConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_words() {
        assert_eq!(classify_word("xin"), Some(WordClass::Latin));
        assert_eq!(classify_word("chào"), Some(WordClass::Vi));
        assert_eq!(classify_word("đó"), Some(WordClass::Vi));
        assert_eq!(classify_word("café"), Some(WordClass::Latin));
        assert_eq!(classify_word("2026."), None);
    }

    #[test]
    fn detects_english() {
        let mut d = LangDetector::default();
        assert_eq!(d.push("Good evening ladies and gentlemen, "), Some(Lang::En));
        assert_eq!(d.current(), Some(Lang::En));
    }

    #[test]
    fn detects_vietnamese_quickly() {
        let mut d = LangDetector::default();
        d.set_current(Some(Lang::En));
        // Two marked words in a row flip immediately.
        assert_eq!(d.push("Xin chào quý vị"), Some(Lang::Vi));
    }

    #[test]
    fn handles_words_split_across_deltas() {
        let mut d = LangDetector::default();
        d.push("Wel");
        d.push("come to");
        d.push(" the con");
        assert_eq!(d.push("ference tonight "), Some(Lang::En));
    }

    #[test]
    fn hysteresis_tolerates_loanwords() {
        let mut d = LangDetector::default();
        d.push("Chúng tôi rất vui được giới thiệu ");
        assert_eq!(d.current(), Some(Lang::Vi));
        // An English brand name inside Vietnamese speech does not flip.
        assert_eq!(d.push("OpenAI và "), None);
        assert_eq!(d.current(), Some(Lang::Vi));
    }

    #[test]
    fn switches_back_to_english_after_enough_words() {
        let mut d = LangDetector::default();
        d.push("Cảm ơn các bạn rất nhiều ");
        assert_eq!(d.current(), Some(Lang::Vi));
        d.reset_window();
        assert_eq!(
            d.push("Thank you so much for being here tonight "),
            Some(Lang::En)
        );
    }

    #[test]
    fn strong_hint_examples() {
        assert_eq!(strong_hint("chào mừng"), Some(Lang::Vi));
        assert_eq!(strong_hint("this is a test"), Some(Lang::En));
        assert_eq!(strong_hint("the"), None);
    }

    #[test]
    fn lang_codes() {
        assert_eq!(Lang::from_code("en-US"), Some(Lang::En));
        assert_eq!(Lang::from_code("VI"), Some(Lang::Vi));
        assert_eq!(Lang::from_code("ja"), None);
        assert_eq!(Lang::Vi.opposite(), Lang::En);
    }
}
