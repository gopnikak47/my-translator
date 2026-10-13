//! Incremental caption track.
//!
//! A track holds the text for ONE display language. Text arrives in three forms:
//!
//! * **stable appends** — OpenAI transcript deltas are append-only by contract,
//!   and Soniox final tokens never change once emitted. Stable text is never
//!   rewritten, so it cannot flicker.
//! * **tentative text** — Soniox non-final tokens, replaced wholesale on each
//!   update. Kept in its own slot so only that tail can change on screen.
//! * **segment ends** — explicit boundaries (Soniox `<end>`, language switch,
//!   idle timeout, or a legacy `*.done` event).
//!
//! Stable text accumulates in `open` (the sentence in progress). Whenever a
//! sentence terminator followed by whitespace appears, the completed sentence
//! moves to `done` as a finalised [`Segment`]. Over-long unpunctuated runs are
//! split at a soft break so a single segment never grows without bound.

use crate::lang::Lang;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::Instant;

/// Finalised segments kept per track (older ones are only in the transcript log).
const MAX_DONE: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segment {
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub lang: Lang,
    done: VecDeque<Segment>,
    open: String,
    tentative: String,
    next_id: u64,
    /// Last time stable text was appended (drives idle commit).
    last_append: Option<Instant>,
    /// Last time anything visible changed (drives hold/expiry).
    last_change: Option<Instant>,
    max_segment_chars: usize,
}

impl Track {
    pub fn new(lang: Lang, max_segment_chars: usize) -> Self {
        Self {
            lang,
            done: VecDeque::new(),
            open: String::new(),
            tentative: String::new(),
            next_id: 1,
            last_append: None,
            last_change: None,
            max_segment_chars,
        }
    }

    pub fn set_max_segment_chars(&mut self, n: usize) {
        self.max_segment_chars = n.max(20);
    }

    pub fn open(&self) -> &str {
        &self.open
    }

    pub fn tentative(&self) -> &str {
        &self.tentative
    }

    pub fn done(&self) -> impl DoubleEndedIterator<Item = &Segment> {
        self.done.iter()
    }

    /// Id the next finalised segment will receive. Screens use it as a
    /// "show only what comes after this point" marker.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    pub fn last_append(&self) -> Option<Instant> {
        self.last_append
    }

    pub fn last_change(&self) -> Option<Instant> {
        self.last_change
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty() && self.tentative.is_empty()
    }

    /// Append stable text. Returns segments that became final.
    pub fn append(&mut self, text: &str, now: Instant) -> Vec<Segment> {
        if text.is_empty() {
            return Vec::new();
        }
        // Deltas are concatenated verbatim (the API forbids inserting spaces
        // between deltas); only leading whitespace of a new sentence is dropped.
        if self.open.is_empty() {
            self.open.push_str(text.trim_start());
        } else {
            self.open.push_str(text);
        }
        self.last_append = Some(now);
        self.last_change = Some(now);
        let mut out = Vec::new();
        while let Some(split) = find_sentence_boundary(&self.open) {
            let rest = self.open.split_off(split);
            let sentence = std::mem::replace(&mut self.open, rest.trim_start().to_string());
            if let Some(seg) = self.push_done(sentence) {
                out.push(seg);
            }
        }
        while self.open.chars().count() > self.max_segment_chars {
            match soft_break(&self.open, self.max_segment_chars) {
                Some(split) => {
                    let rest = self.open.split_off(split);
                    let head = std::mem::replace(&mut self.open, rest.trim_start().to_string());
                    if let Some(seg) = self.push_done(head) {
                        out.push(seg);
                    }
                }
                None => break,
            }
        }
        out
    }

    /// Replace the revisable tail (Soniox non-final tokens).
    pub fn set_tentative(&mut self, text: &str, now: Instant) {
        let t = if self.open.is_empty() { text.trim_start() } else { text };
        if t != self.tentative {
            self.tentative.clear();
            self.tentative.push_str(t);
            self.last_change = Some(now);
        }
    }

    /// Close the sentence in progress (tentative text is discarded: it was
    /// never confirmed). Returns the finalised segment, if any.
    pub fn end_segment(&mut self, now: Instant) -> Option<Segment> {
        if !self.tentative.is_empty() {
            self.tentative.clear();
            self.last_change = Some(now);
        }
        if self.open.trim().is_empty() {
            self.open.clear();
            return None;
        }
        let text = std::mem::take(&mut self.open);
        self.last_change = Some(now);
        self.push_done(text)
    }

    /// Handle an authoritative "full text of the segment" event (legacy
    /// `*.done` events). Never duplicates text that was already streamed.
    pub fn finalize_with(&mut self, full: &str, now: Instant) -> Vec<Segment> {
        let full_n = normalize(full);
        if full_n.is_empty() {
            return self.end_segment(now).into_iter().collect();
        }
        let open_n = normalize(&self.open);
        // Recent finalised text + the open sentence, as streamed.
        let mut recent: Vec<&str> = self.done.iter().rev().take(6).map(|s| s.text.as_str()).collect();
        recent.reverse();
        let mut streamed = recent.join(" ");
        if !self.open.is_empty() {
            streamed.push(' ');
            streamed.push_str(&self.open);
        }
        let streamed_n = normalize(&streamed);

        if !open_n.is_empty() && full_n.starts_with(&open_n) && full_n.len() > open_n.len() && !streamed_n.ends_with(&full_n) {
            // The final text extends what we streamed: adopt it.
            self.open = full.trim().to_string();
            self.tentative.clear();
            return self.end_segment(now).into_iter().collect();
        }
        if streamed_n.contains(&full_n) || full_n.ends_with(&open_n) && !open_n.is_empty() {
            // Already shown (possibly split into several sentences): just close.
            return self.end_segment(now).into_iter().collect();
        }
        if open_n.is_empty() {
            // Nothing streamed for it (deltas lost): add it as its own segment.
            self.tentative.clear();
            self.last_change = Some(now);
            return self.push_done(full.trim().to_string()).into_iter().collect();
        }
        // Diverging text: keep what the audience already saw, never show both.
        self.end_segment(now).into_iter().collect()
    }

    /// Remove `text` from the end of the in-progress sentence if it is still
    /// there (used to retract a same-language echo detected after the fact).
    pub fn retract_suffix(&mut self, text: &str, now: Instant) -> bool {
        if text.is_empty() || !self.open.ends_with(text) {
            return false;
        }
        let keep = self.open.len() - text.len();
        self.open.truncate(keep);
        self.open.truncate(self.open.trim_end().len());
        self.last_change = Some(now);
        true
    }

    /// Drop everything (operator "clear").
    pub fn clear(&mut self, now: Instant) {
        self.open.clear();
        self.tentative.clear();
        self.last_change = Some(now);
    }

    fn push_done(&mut self, text: String) -> Option<Segment> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let seg = Segment { id: self.next_id, text };
        self.next_id += 1;
        self.done.push_back(seg.clone());
        while self.done.len() > MAX_DONE {
            self.done.pop_front();
        }
        Some(seg)
    }
}

const TERMINATORS: [char; 6] = ['.', '!', '?', '…', '。', '؟'];
const CLOSERS: [char; 5] = ['"', '\'', ')', '”', '’'];
/// Abbreviations that end with a period but do not end a sentence.
const ABBREVIATIONS: [&str; 16] = [
    "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "vs", "etc", "e.g", "i.e", "u.s", "tp", "ths", "ts",
];

/// Byte index just after the first sentence end (terminator, optional closing
/// quote/bracket) that is followed by whitespace. Terminators at the very end
/// of the buffer are NOT boundaries yet: the next delta may continue ("3.5").
pub fn find_sentence_boundary(s: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (_, c) = chars[i];
        if TERMINATORS.contains(&c) {
            let mut j = i + 1;
            while j < chars.len() && (TERMINATORS.contains(&chars[j].1) || CLOSERS.contains(&chars[j].1)) {
                j += 1;
            }
            if j < chars.len() && chars[j].1.is_whitespace() {
                let end = chars[j].0;
                if c == '.' && is_abbreviation(&s[..chars[i].0]) {
                    i = j;
                    continue;
                }
                return Some(end);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

fn is_abbreviation(before_dot: &str) -> bool {
    let word: String = before_dot
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if word.is_empty() {
        return false;
    }
    // Single letters ("J. K. Rowling") and known abbreviations.
    (word.chars().count() == 1 && word.chars().all(|c| c.is_alphabetic()))
        || ABBREVIATIONS.contains(&word.as_str())
}

/// Split point for an over-long run: prefer the last clause break (",;:—–")
/// before `limit` chars, else the last space. Byte index into `s`.
fn soft_break(s: &str, limit: usize) -> Option<usize> {
    let mut last_clause = None;
    let mut last_space = None;
    for (count, (idx, c)) in s.char_indices().enumerate() {
        if count >= limit {
            break;
        }
        if c.is_whitespace() && count > 0 {
            last_space = Some(idx);
            let prev = s[..idx].chars().last();
            if matches!(prev, Some(',' | ';' | ':' | '—' | '–')) {
                last_clause = Some(idx);
            }
        }
    }
    // Avoid absurdly short heads from an early comma.
    match last_clause {
        Some(i) if i >= limit / 3 => Some(i),
        _ => last_space.filter(|&i| i > 0),
    }
}

/// Lowercase, collapse whitespace, strip punctuation spacing differences.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&w.to_lowercase());
    }
    out
}

/// Cut `text` from the front so at most `max_chars` characters remain, at a
/// word boundary. Returns a slice of `text`.
pub fn tail_chars(text: &str, max_chars: usize) -> &str {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let skip = total - max_chars;
    let start = text.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    let tail = &text[start..];
    match tail.find(char::is_whitespace) {
        Some(ws) if ws + 1 < tail.len() => tail[ws..].trim_start(),
        _ => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Track {
        Track::new(Lang::Vi, 140)
    }

    #[test]
    fn deltas_concatenate_without_extra_spaces() {
        let mut tr = t();
        let now = Instant::now();
        for d in ["Xin", " chà", "o", " quý", " vị"] {
            tr.append(d, now);
        }
        assert_eq!(tr.open(), "Xin chào quý vị");
    }

    #[test]
    fn sentence_completes_only_when_followed_by_space() {
        let mut tr = t();
        let now = Instant::now();
        assert!(tr.append("Giá là 3.", now).is_empty());
        assert!(tr.append("5 triệu.", now).is_empty());
        let segs = tr.append(" Cảm ơn", now);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "Giá là 3.5 triệu.");
        assert_eq!(tr.open(), "Cảm ơn");
    }

    #[test]
    fn multiple_sentences_in_one_delta_keep_order() {
        let mut tr = t();
        let segs = tr.append("One. Two! Three? Four", Instant::now());
        let texts: Vec<_> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["One.", "Two!", "Three?"]);
        assert_eq!(tr.open(), "Four");
        let ids: Vec<_> = segs.iter().map(|s| s.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn abbreviations_do_not_split() {
        let mut tr = t();
        let segs = tr.append("Welcome Dr. Smith and Mr. Lee to the U.S. office. Next", Instant::now());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "Welcome Dr. Smith and Mr. Lee to the U.S. office.");
    }

    #[test]
    fn closing_quote_belongs_to_sentence() {
        let mut tr = t();
        let segs = tr.append("He said \"hello.\" Then", Instant::now());
        assert_eq!(segs[0].text, "He said \"hello.\"");
    }

    #[test]
    fn long_runs_are_split_at_soft_breaks() {
        let mut tr = Track::new(Lang::En, 40);
        let segs = tr.append(
            "this sentence is quite long, so it keeps going and going and going on and on",
            Instant::now(),
        );
        assert!(!segs.is_empty());
        for s in &segs {
            assert!(s.text.chars().count() <= 40, "{}", s.text);
        }
        assert!(segs[0].text.ends_with(','));
    }

    #[test]
    fn tentative_is_replaced_and_dropped_on_end() {
        let mut tr = t();
        let now = Instant::now();
        tr.append("Hello", now);
        tr.set_tentative(" wor", now);
        tr.set_tentative(" world", now);
        assert_eq!(tr.tentative(), " world");
        let seg = tr.end_segment(now).unwrap();
        assert_eq!(seg.text, "Hello");
        assert_eq!(tr.tentative(), "");
    }

    #[test]
    fn done_event_matching_stream_is_not_duplicated() {
        let mut tr = t();
        let now = Instant::now();
        tr.append("Hello everyone. How are", now);
        tr.append(" you", now);
        let segs = tr.finalize_with("Hello everyone. How are you", now);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "How are you");
        let all: Vec<_> = tr.done().map(|s| s.text.clone()).collect();
        assert_eq!(all, ["Hello everyone.", "How are you"]);
        // A repeated done for the same text is ignored entirely.
        assert!(tr.finalize_with("How are you", now).is_empty());
        assert_eq!(tr.done().count(), 2);
    }

    #[test]
    fn done_event_extending_stream_is_adopted() {
        let mut tr = t();
        let now = Instant::now();
        tr.append("Thank you", now);
        let segs = tr.finalize_with("Thank you all", now);
        assert_eq!(segs[0].text, "Thank you all");
    }

    #[test]
    fn done_event_without_deltas_is_added_once() {
        let mut tr = t();
        let now = Instant::now();
        let segs = tr.finalize_with("Lost deltas text", now);
        assert_eq!(segs.len(), 1);
        assert!(tr.finalize_with("Lost deltas text", now).is_empty());
    }

    #[test]
    fn tail_chars_cuts_on_word_boundary() {
        assert_eq!(tail_chars("one two three four", 9), "four");
        assert_eq!(tail_chars("short", 10), "short");
        assert_eq!(tail_chars("xin chào quý vị", 8), "quý vị");
    }

    #[test]
    fn done_queue_is_bounded() {
        let mut tr = t();
        let now = Instant::now();
        for i in 0..100 {
            tr.append(&format!("Sentence {i}. "), now);
        }
        assert!(tr.done().count() <= MAX_DONE);
        assert_eq!(tr.done().last().unwrap().text, "Sentence 99.");
    }
}
