//! Bilingual caption router.
//!
//! Keeps one [`Track`] per display language (EN, VI) and decides which incoming
//! text goes where:
//!
//! * Source-transcript text goes to the track of the language being spoken —
//!   so a screen whose language equals the spoken language shows the original
//!   words instead of a pointless same-language "translation".
//! * Translated text goes to the track of its target language, unless the target
//!   equals the language being spoken (an echo from the dual-session strategy).
//!   Echo candidates are held for `echo_hold_ms` rather than dropped at once,
//!   because translation deltas can arrive slightly before the transcript reveals
//!   a language switch; if the switch is confirmed within the window the held
//!   text is released into the right track, otherwise it is discarded.
//!
//! Screens then pick a track: display mode A (`Opposite`) follows the opposite
//! of the spoken language, display mode B (`Fixed`) always shows one language.

use crate::captions::{tail_chars, Segment, Track};
use crate::config::{CaptionConfig, FinalMode, ScreenLanguage, TranslationMode};
use crate::lang::{is_vi_char, strong_hint, Lang, LangDetector};
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A long pause in the source transcript starts a new detection window, so a
/// new speaker's language is recognised after a few words.
const PAUSE_RESET: Duration = Duration::from_millis(1500);
/// Ambiguous (unaccented) source words held while Vietnamese is spoken.
const PENDING_SOURCE_WORDS: usize = 6;
const PENDING_SOURCE_MAX_AGE: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenId {
    Left,
    Right,
}

impl ScreenId {
    pub const ALL: [ScreenId; 2] = [ScreenId::Left, ScreenId::Right];

    pub fn index(self) -> usize {
        match self {
            ScreenId::Left => 0,
            ScreenId::Right => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ScreenId::Left => "left",
            ScreenId::Right => "right",
        }
    }
}

/// What one overlay screen should display right now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScreenFrame {
    pub screen: ScreenId,
    pub seq: u64,
    pub lang: Option<Lang>,
    /// Text that will not change any more.
    pub stable: String,
    /// Revisable tail (Soniox non-final tokens); empty for OpenAI.
    pub tentative: String,
    pub visible: bool,
    /// Publish time (Unix ms) for overlay latency measurement.
    pub t_pub_ms: u64,
}

impl ScreenFrame {
    pub fn empty(screen: ScreenId) -> Self {
        Self {
            screen,
            seq: 0,
            lang: None,
            stable: String::new(),
            tentative: String::new(),
            visible: true,
            t_pub_ms: 0,
        }
    }

    /// Equality ignoring sequence/timestamps (used to suppress no-op pushes).
    pub fn same_content(&self, other: &ScreenFrame) -> bool {
        self.lang == other.lang
            && self.stable == other.stable
            && self.tentative == other.tentative
            && self.visible == other.visible
    }
}

/// A finalised caption segment for the operator log / transcript.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CompletedSegment {
    pub lang: Lang,
    pub spoken: Lang,
    /// "original" when `lang == spoken`, else "translation".
    pub kind: &'static str,
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Hide,
    Show,
    ToggleHidden,
    Freeze,
    Unfreeze,
    ToggleFrozen,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Source,
    Translation,
}

#[derive(Debug, Clone)]
struct HeldDelta {
    at: Instant,
    target: Lang,
    text: String,
}

#[derive(Debug, Clone)]
struct ScreenState {
    language: ScreenLanguage,
    /// Track currently shown and the first segment id to show from it.
    showing: Option<Lang>,
    since: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouterStatus {
    pub mode: TranslationMode,
    pub spoken: Lang,
    pub detected: Option<Lang>,
    pub override_lang: Option<Lang>,
    pub hidden: bool,
    pub frozen: bool,
    pub screen_langs: [Option<Lang>; 2],
}

pub struct CaptionRouter {
    cfg: CaptionConfig,
    mode: TranslationMode,
    tracks: [Track; 2],
    detector: LangDetector,
    spoken: Lang,
    override_lang: Option<Lang>,
    /// Whether a source-transcript stream feeds this router (always true for
    /// the OpenAI engine and Soniox). Without one, same-language output is
    /// shown rather than held as an echo.
    has_source_stream: bool,
    /// Translation text appended recently, per target language, so it can be
    /// retracted if a language switch reveals it was a same-language echo.
    recent_translation: VecDeque<HeldDelta>,
    /// Source text of uncertain language awaiting routing.
    pending_src: VecDeque<HeldDelta>,
    held: VecDeque<HeldDelta>,
    screens: [ScreenState; 2],
    pub hidden: bool,
    pub frozen: bool,
    last_source_at: Option<Instant>,
    completed: Vec<CompletedSegment>,
    echo_dropped_chars: u64,
}

impl CaptionRouter {
    pub fn new(cfg: CaptionConfig, mode: TranslationMode, default_spoken: Lang) -> Self {
        let spoken = mode.fixed_source().unwrap_or(default_spoken);
        let mut detector = LangDetector::default();
        detector.set_current(Some(spoken));
        let max = cfg.max_segment_chars;
        Self {
            cfg,
            mode,
            tracks: [Track::new(Lang::En, max), Track::new(Lang::Vi, max)],
            detector,
            spoken,
            override_lang: None,
            has_source_stream: true,
            recent_translation: VecDeque::new(),
            pending_src: VecDeque::new(),
            held: VecDeque::new(),
            screens: [
                ScreenState { language: ScreenLanguage::Opposite, showing: Some(spoken.opposite()), since: 0 },
                ScreenState { language: ScreenLanguage::Opposite, showing: Some(spoken.opposite()), since: 0 },
            ],
            hidden: false,
            frozen: false,
            last_source_at: None,
            completed: Vec::new(),
            echo_dropped_chars: 0,
        }
    }

    pub fn set_source_stream(&mut self, present: bool) {
        self.has_source_stream = present;
    }

    pub fn config(&self) -> &CaptionConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: CaptionConfig) {
        for t in self.tracks.iter_mut() {
            t.set_max_segment_chars(cfg.max_segment_chars);
        }
        self.cfg = cfg;
    }

    pub fn set_screen_language(&mut self, screen: ScreenId, language: ScreenLanguage) {
        if self.screens[screen.index()].language == language {
            return;
        }
        self.screens[screen.index()].language = language;
        // A screen that changes language starts clean from this moment.
        if let Some(lang) = self.screen_lang(screen) {
            let since = self.tracks[lang.index()].next_id();
            let st = &mut self.screens[screen.index()];
            if st.showing != Some(lang) {
                st.showing = Some(lang);
                st.since = since;
            }
        }
    }

    pub fn mode(&self) -> TranslationMode {
        self.mode
    }

    /// Change translation mode (keeps on-screen text, resets detection).
    pub fn set_mode(&mut self, mode: TranslationMode, default_spoken: Lang, now: Instant) {
        self.mode = mode;
        let spoken = self.override_lang.or(mode.fixed_source()).unwrap_or(default_spoken);
        self.detector.reset_window();
        self.detector.set_current(Some(spoken));
        if spoken != self.spoken {
            self.switch_spoken(spoken, now);
        }
    }

    pub fn spoken(&self) -> Lang {
        self.spoken
    }

    pub fn override_lang(&self) -> Option<Lang> {
        self.override_lang
    }

    pub fn status(&self) -> RouterStatus {
        RouterStatus {
            mode: self.mode,
            spoken: self.spoken,
            detected: self.detector.current(),
            override_lang: self.override_lang,
            hidden: self.hidden,
            frozen: self.frozen,
            screen_langs: [self.screen_lang(ScreenId::Left), self.screen_lang(ScreenId::Right)],
        }
    }

    pub fn take_completed(&mut self) -> Vec<CompletedSegment> {
        std::mem::take(&mut self.completed)
    }

    pub fn take_echo_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.echo_dropped_chars)
    }

    /// Whether auto-detection currently decides the spoken language.
    fn detecting(&self) -> bool {
        self.mode == TranslationMode::Auto && self.override_lang.is_none()
    }

    /// Manual operator override of the spoken language (`None` = automatic).
    /// Returns the new spoken language if it changed.
    pub fn set_override(&mut self, lang: Option<Lang>, now: Instant) -> Option<Lang> {
        self.override_lang = lang;
        let target = match lang {
            Some(l) => l,
            None => match self.mode.fixed_source() {
                Some(l) => l,
                None => self.detector.current().unwrap_or(self.spoken),
            },
        };
        if let Some(l) = lang {
            self.detector.set_current(Some(l));
        }
        if target != self.spoken {
            self.switch_spoken(target, now);
            Some(target)
        } else {
            None
        }
    }

    /// Source-language transcript delta (append-only). Returns the new spoken
    /// language when this delta caused a switch.
    pub fn source_delta(&mut self, text: &str, now: Instant) -> Option<Lang> {
        if text.is_empty() {
            return None;
        }
        self.has_source_stream = true;
        if let Some(prev) = self.last_source_at {
            if now.duration_since(prev) >= PAUSE_RESET {
                self.detector.reset_window();
            }
        }
        self.last_source_at = Some(now);
        let mut switched = None;
        if self.detecting() {
            if let Some(l) = self.detector.push(text) {
                if l != self.spoken {
                    self.switch_spoken(l, now);
                    switched = Some(l);
                }
            }
        }
        if self.detecting() {
            let marked_vi = text.chars().any(is_vi_char);
            // While Vietnamese is spoken, unaccented words may be the start of
            // English speech: hold them briefly until the language is clear
            // (a marked word confirms Vietnamese, a detector flip confirms
            // English) so they land on the right screen.
            if self.spoken == Lang::Vi && !marked_vi && (!self.pending_src.is_empty() || text.chars().any(char::is_alphabetic)) {
                self.pending_src.push_back(HeldDelta { at: now, target: Lang::Vi, text: text.to_string() });
                let words: usize = self.pending_src.iter().map(|h| h.text.split_whitespace().count()).sum();
                if words > PENDING_SOURCE_WORDS {
                    self.flush_pending_src(Lang::Vi, now);
                }
                return switched;
            }
            // Route a clearly-marked fragment by its own language even if the
            // window has not flipped yet (avoids Vietnamese words on the EN track).
            let route = strong_hint(text).filter(|l| *l == Lang::Vi).unwrap_or(self.spoken);
            self.flush_pending_src(route, now);
            self.append(route, text, now);
        } else {
            self.append(self.spoken, text, now);
        }
        switched
    }

    fn flush_pending_src(&mut self, lang: Lang, now: Instant) {
        let pending: Vec<HeldDelta> = self.pending_src.drain(..).collect();
        for h in pending {
            self.append(lang, &h.text, now);
        }
    }

    /// Translated-output delta for `target` (append-only).
    pub fn translation_delta(&mut self, target: Lang, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        if self.has_source_stream && target == self.spoken {
            self.held.push_back(HeldDelta { at: now, target, text: text.to_string() });
            return;
        }
        self.append(target, text, now);
        if self.has_source_stream && self.detecting() {
            self.recent_translation.push_back(HeldDelta { at: now, target, text: text.to_string() });
            while self.recent_translation.len() > 64 {
                self.recent_translation.pop_front();
            }
        }
    }

    /// Authoritative full text for a translated segment (legacy `.done`).
    pub fn translation_done(&mut self, target: Lang, text: &str, now: Instant) {
        if self.has_source_stream && target == self.spoken {
            return;
        }
        let segs = self.tracks[target.index()].finalize_with(text, now);
        self.record_completed(target, segs);
    }

    /// Soniox-style input: stable text + replaceable tentative tail. `lang` is
    /// the token language reported by the provider.
    pub fn ingest_tokens(
        &mut self,
        stream: Stream,
        lang: Option<Lang>,
        stable: &str,
        tentative: &str,
        end: bool,
        now: Instant,
    ) {
        let route = match stream {
            Stream::Source => {
                if let (true, Some(l)) = (self.detecting(), lang) {
                    if l != self.spoken && (!stable.trim().is_empty() || !tentative.trim().is_empty()) {
                        self.switch_spoken(l, now);
                    }
                }
                self.has_source_stream = true;
                if self.detecting() {
                    lang.unwrap_or(self.spoken)
                } else {
                    self.spoken
                }
            }
            Stream::Translation => match lang {
                Some(l) => l,
                None => self.spoken.opposite(),
            },
        };
        if !stable.is_empty() {
            self.append(route, stable, now);
        }
        let tr = &mut self.tracks[route.index()];
        tr.set_tentative(tentative, now);
        if end {
            let seg = tr.end_segment(now);
            self.record_completed(route, seg.into_iter().collect());
        }
    }

    /// Close the in-progress sentence on all tracks.
    pub fn end_all_segments(&mut self, now: Instant) {
        let lang = self.spoken;
        self.flush_pending_src(lang, now);
        for l in Lang::ALL {
            let seg = self.tracks[l.index()].end_segment(now);
            self.record_completed(l, seg.into_iter().collect());
        }
    }

    pub fn control(&mut self, c: Control, now: Instant) {
        match c {
            Control::Hide => self.hidden = true,
            Control::Show => self.hidden = false,
            Control::ToggleHidden => self.hidden = !self.hidden,
            Control::Freeze => self.frozen = true,
            Control::Unfreeze => self.frozen = false,
            Control::ToggleFrozen => self.frozen = !self.frozen,
            Control::Clear => {
                self.end_all_segments(now);
                for t in self.tracks.iter_mut() {
                    t.clear(now);
                }
                self.held.clear();
                self.pending_src.clear();
                for s in ScreenId::ALL {
                    let lang = self.screen_lang(s);
                    let st = &mut self.screens[s.index()];
                    st.showing = lang;
                    st.since = lang.map(|l| self.tracks[l.index()].next_id()).unwrap_or(0);
                }
            }
        }
    }

    /// Periodic housekeeping: echo expiry, idle commit, caption hold expiry.
    pub fn tick(&mut self, now: Instant) {
        if self.pending_src.front().map(|h| now.duration_since(h.at) >= PENDING_SOURCE_MAX_AGE).unwrap_or(false) {
            let lang = self.spoken;
            self.flush_pending_src(lang, now);
        }
        let hold = Duration::from_millis(self.cfg.echo_hold_ms);
        while let Some(h) = self.held.front() {
            if now.duration_since(h.at) >= hold {
                let h = self.held.pop_front().unwrap();
                self.echo_dropped_chars += h.text.chars().count() as u64;
            } else {
                break;
            }
        }
        let idle = Duration::from_millis(self.cfg.idle_commit_ms);
        for l in Lang::ALL {
            let t = &mut self.tracks[l.index()];
            if !t.open().is_empty() || !t.tentative().is_empty() {
                if let Some(last) = t.last_change() {
                    if now.duration_since(last) >= idle {
                        let seg = t.end_segment(now);
                        self.record_completed(l, seg.into_iter().collect());
                    }
                }
            }
        }
        if self.cfg.hold_ms > 0 {
            let hold = Duration::from_millis(self.cfg.hold_ms);
            for s in ScreenId::ALL {
                let Some(lang) = self.screen_lang(s) else { continue };
                let t = &self.tracks[lang.index()];
                let stale = t.last_change().map(|c| now.duration_since(c) >= hold).unwrap_or(false);
                if stale && t.is_empty() {
                    let st = &mut self.screens[s.index()];
                    if st.showing == Some(lang) && st.since < t.next_id() {
                        st.since = t.next_id();
                    }
                }
            }
        }
    }

    /// Language a screen currently displays.
    pub fn screen_lang(&self, screen: ScreenId) -> Option<Lang> {
        match self.screens[screen.index()].language {
            ScreenLanguage::Fixed(l) => Some(l),
            ScreenLanguage::Opposite => Some(self.spoken.opposite()),
        }
    }

    /// Compute what a screen shows (sequence/timestamp filled in by the hub).
    pub fn render(&mut self, screen: ScreenId) -> ScreenFrame {
        let lang = self.screen_lang(screen);
        let mut frame = ScreenFrame::empty(screen);
        frame.lang = lang;
        frame.visible = !self.hidden;
        let Some(lang) = lang else { return frame };
        let track = &self.tracks[lang.index()];
        let st = &mut self.screens[screen.index()];
        if st.showing != Some(lang) {
            // Screen switched language (mode A follows the speaker): start clean
            // rather than showing old text from that track.
            st.showing = Some(lang);
            st.since = track.next_id();
        }
        let since = st.since;
        let cfg = &self.cfg;
        let visible_done: Vec<&Segment> = track.done().filter(|s| s.id >= since).collect();
        let mut stable = String::new();
        let mut tentative = String::new();
        match cfg.final_mode {
            FinalMode::Rollup => {
                for s in &visible_done {
                    push_word_sep(&mut stable, &s.text);
                }
                if cfg.show_partials {
                    push_word_sep(&mut stable, track.open());
                    tentative.push_str(track.tentative());
                }
            }
            FinalMode::Replace => {
                let partial_present = cfg.show_partials && (!track.open().is_empty() || !track.tentative().is_empty());
                if partial_present {
                    stable.push_str(track.open());
                    tentative.push_str(track.tentative());
                } else if let Some(last) = visible_done.last() {
                    stable.push_str(&last.text);
                }
            }
        }
        // Bound payload: keep the newest text (the overlay clips to max lines).
        let total = stable.chars().count() + tentative.chars().count();
        if total > cfg.max_chars {
            let tent_len = tentative.chars().count();
            if tent_len >= cfg.max_chars {
                tentative = tail_chars(&tentative, cfg.max_chars).to_string();
                stable.clear();
            } else {
                stable = tail_chars(&stable, cfg.max_chars - tent_len).to_string();
            }
        }
        if tentative.is_empty() {
            stable.truncate(stable.trim_end().len());
        }
        frame.stable = stable;
        frame.tentative = tentative;
        frame
    }

    fn append(&mut self, lang: Lang, text: &str, now: Instant) {
        let segs = self.tracks[lang.index()].append(text, now);
        self.record_completed(lang, segs);
    }

    fn switch_spoken(&mut self, new: Lang, now: Instant) {
        // Translation into `new` that arrived just before the switch was really a
        // same-language echo of the new speech: retract it while it is still the
        // unfinished tail, so the original transcript is not shown twice.
        let window = Duration::from_millis(self.cfg.echo_hold_ms);
        let recent: Vec<HeldDelta> = self.recent_translation.drain(..).collect();
        for h in recent.iter().rev() {
            if h.target == new && now.duration_since(h.at) <= window {
                if !self.tracks[new.index()].retract_suffix(&h.text, now) {
                    break;
                }
                self.echo_dropped_chars += h.text.chars().count() as u64;
            }
        }
        // Uncertain source words were the start of the new language.
        let pending: Vec<HeldDelta> = self.pending_src.drain(..).collect();
        let old = self.spoken;
        self.spoken = new;
        // A language change is a hard sentence boundary on every track.
        self.end_all_segments(now);
        // Mode-A screens now follow another track: show only what comes next.
        for s in ScreenId::ALL {
            if self.screens[s.index()].language == ScreenLanguage::Opposite {
                let lang = new.opposite();
                let since = self.tracks[lang.index()].next_id();
                let st = &mut self.screens[s.index()];
                st.showing = Some(lang);
                st.since = since;
            }
        }
        for h in pending {
            self.append(new, &h.text, now);
        }
        // Held text targets the previous spoken language. Echoes of the old
        // speech come first, genuine translations of the new speech after them:
        // drop only a leading run that clearly repeats the recent source
        // (≥3 words, ≥80 % matching), release everything else in order.
        let held: Vec<HeldDelta> = self.held.drain(..).filter(|h| h.target != new).collect();
        if !held.is_empty() {
            let mut cache = Vec::new();
            let counts: Vec<(usize, usize)> = held.iter().map(|h| self.echo_word_hits(old, &h.text, &mut cache)).collect();
            let mut best_k = 0;
            let mut best_score: isize = counts.iter().map(|(hit, tot)| (tot - hit) as isize).sum();
            let (mut pre_hit, mut pre_tot) = (0usize, 0usize);
            for k in 1..=held.len() {
                pre_hit += counts[k - 1].0;
                pre_tot += counts[k - 1].1;
                if pre_tot < 3 || pre_hit * 10 < pre_tot * 8 {
                    continue;
                }
                let suffix: isize = counts[k..].iter().map(|(hit, tot)| (tot - hit) as isize).sum();
                let score = pre_hit as isize + suffix;
                if score >= best_score + 2 {
                    best_score = score;
                    best_k = k;
                }
            }
            for (i, h) in held.into_iter().enumerate() {
                if i < best_k {
                    self.echo_dropped_chars += h.text.chars().count() as u64;
                } else {
                    self.append(h.target, &h.text, now);
                }
            }
        }
    }

    /// (matching words, words) of `text` against the latest source transcript of `lang`.
    fn echo_word_hits(&self, lang: Lang, text: &str, cache: &mut Vec<String>) -> (usize, usize) {
        if cache.is_empty() {
            let t = &self.tracks[lang.index()];
            let mut recent: Vec<&str> = t.done().rev().take(4).map(|s| s.text.as_str()).collect();
            recent.reverse();
            recent.push(t.open());
            cache.extend(recent.join(" ").split_whitespace().map(normalize_word));
            if cache.is_empty() {
                cache.push(String::new());
            }
        }
        let words: Vec<String> = text.split_whitespace().map(normalize_word).filter(|w| !w.is_empty()).collect();
        (words.iter().filter(|w| cache.contains(w)).count(), words.len())
    }

    fn record_completed(&mut self, lang: Lang, segs: Vec<Segment>) {
        for s in segs {
            self.completed.push(CompletedSegment {
                lang,
                spoken: self.spoken,
                kind: if lang == self.spoken { "original" } else { "translation" },
                id: s.id,
                text: s.text,
            });
        }
    }
}

fn normalize_word(w: &str) -> String {
    w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

fn push_word_sep(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with(char::is_whitespace) && !text.starts_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TentativeStyle;

    fn cfg() -> CaptionConfig {
        CaptionConfig { tentative_style: TentativeStyle::Same, ..CaptionConfig::default() }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn en_to_vi_shows_translation_on_opposite_screens() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::EnToVi, Lang::En);
        let now = Instant::now();
        r.source_delta("Good evening everyone", now);
        r.translation_delta(Lang::Vi, "Chào buổi tối", now);
        r.translation_delta(Lang::Vi, " mọi người", now);
        let f = r.render(ScreenId::Left);
        assert_eq!(f.lang, Some(Lang::Vi));
        assert_eq!(f.stable, "Chào buổi tối mọi người");
        assert_eq!(r.render(ScreenId::Right).stable, "Chào buổi tối mọi người");
    }

    #[test]
    fn vi_to_en_shows_english() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::ViToEn, Lang::En);
        let now = Instant::now();
        r.source_delta("Xin chào", now);
        r.translation_delta(Lang::En, "Hello", now);
        let f = r.render(ScreenId::Left);
        assert_eq!(f.lang, Some(Lang::En));
        assert_eq!(f.stable, "Hello");
    }

    #[test]
    fn mode_b_fixed_screens_show_original_for_same_language() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        r.set_screen_language(ScreenId::Left, ScreenLanguage::Fixed(Lang::Vi));
        r.set_screen_language(ScreenId::Right, ScreenLanguage::Fixed(Lang::En));
        let now = Instant::now();
        r.source_delta("Welcome to the annual meeting ", now);
        r.translation_delta(Lang::Vi, "Chào mừng đến với cuộc họp thường niên ", now);
        // Dual-session echo (EN output of English speech) must not appear twice.
        r.translation_delta(Lang::En, "Welcome to the annual meeting ", now);
        let left = r.render(ScreenId::Left);
        let right = r.render(ScreenId::Right);
        assert_eq!(left.stable, "Chào mừng đến với cuộc họp thường niên");
        assert_eq!(right.stable, "Welcome to the annual meeting");
        r.tick(now + ms(2000));
        assert_eq!(r.render(ScreenId::Right).stable, "Welcome to the annual meeting");
    }

    #[test]
    fn auto_mode_switches_language_and_screens_follow() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        let t0 = Instant::now();
        r.source_delta("Thank you all for coming tonight. ", t0);
        r.translation_delta(Lang::Vi, "Cảm ơn tất cả mọi người đã đến tối nay. ", t0);
        assert_eq!(r.render(ScreenId::Left).lang, Some(Lang::Vi));

        let t1 = t0 + ms(3000);
        let switched = r.source_delta("Xin chào quý vị đại biểu ", t1);
        assert_eq!(switched, Some(Lang::Vi));
        r.translation_delta(Lang::En, "Hello distinguished delegates ", t1);
        let f = r.render(ScreenId::Left);
        assert_eq!(f.lang, Some(Lang::En));
        // Screen starts clean after following the switch.
        assert_eq!(f.stable, "Hello distinguished delegates");
    }

    #[test]
    fn early_translation_is_held_then_released_on_switch() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        let t0 = Instant::now();
        r.source_delta("Okay so let us begin ", t0);
        // EN output arrives before the Vietnamese transcript reveals the switch.
        r.translation_delta(Lang::En, "Next, ", t0 + ms(100));
        r.source_delta("Tiếp theo là phần ", t0 + ms(300));
        let f = r.render(ScreenId::Right);
        assert_eq!(f.lang, Some(Lang::En));
        assert_eq!(f.stable, "Next,");
    }

    #[test]
    fn echo_arriving_before_switch_is_retracted() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        r.set_screen_language(ScreenId::Left, ScreenLanguage::Fixed(Lang::Vi));
        let t0 = Instant::now();
        r.source_delta("So that is all from me ", t0);
        r.translation_delta(Lang::Vi, "Vậy là hết phần của tôi. ", t0);
        // Vietnamese speech starts: the →vi session echoes it before the
        // transcript reveals the switch.
        r.translation_delta(Lang::Vi, "Xin chào", t0 + ms(2000));
        r.source_delta("Xin chào quý vị ", t0 + ms(2200));
        let left = r.render(ScreenId::Left);
        assert_eq!(left.stable.matches("Xin chào").count(), 1, "{}", left.stable);
        assert!(left.stable.ends_with("Xin chào quý vị"), "{}", left.stable);
    }

    #[test]
    fn old_echo_is_not_released_by_a_quick_switch() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        let t0 = Instant::now();
        r.source_delta("Thank you all for coming tonight. ", t0);
        r.translation_delta(Lang::En, "Thank you all for coming tonight. ", t0 + ms(50)); // echo, held
        r.translation_delta(Lang::En, "Hello delegates ", t0 + ms(150)); // translation of new speech
        r.source_delta("Xin chào quý vị đại biểu ", t0 + ms(200));
        let left = r.render(ScreenId::Left);
        assert_eq!(left.lang, Some(Lang::En));
        assert_eq!(left.stable, "Hello delegates");
    }

    #[test]
    fn vi_to_en_switch_routes_first_english_words_correctly() {
        // Mode B, word-by-word interleaving exactly as the providers stream it.
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::Vi);
        r.set_screen_language(ScreenId::Left, ScreenLanguage::Fixed(Lang::Vi));
        r.set_screen_language(ScreenId::Right, ScreenLanguage::Fixed(Lang::En));
        let mut t = Instant::now();
        for (src, tr) in [("Xin ", "Thank "), ("trân ", "you "), ("trọng ", "very "), ("cảm ", "much "), ("ơn ", "to all "), ("quý ", "guests. "), ("vị. ", "")] {
            r.source_delta(src, t);
            if !tr.is_empty() {
                r.translation_delta(Lang::En, tr, t);
            }
            t += ms(180);
        }
        t += ms(1600); // pause between speakers
        r.tick(t);
        for (src, tr) in [("Good ", "Xin "), ("evening ", "chào "), ("ladies ", "quý "), ("and ", "vị "), ("gentlemen, ", "và "), ("welcome ", "các bạn, ")] {
            r.source_delta(src, t);
            r.translation_delta(Lang::Vi, tr, t);
            t += ms(180);
        }
        assert_eq!(r.spoken(), Lang::En);
        let left = r.render(ScreenId::Left).stable;
        let right = r.render(ScreenId::Right).stable;
        assert!(left.ends_with("Xin trân trọng cảm ơn quý vị. Xin chào quý vị và các bạn,"), "left: {left}");
        assert!(!left.contains("Good"), "English leaked onto the VI screen: {left}");
        assert!(right.ends_with("Thank you very much to all guests. Good evening ladies and gentlemen, welcome"), "right: {right}");
    }

    #[test]
    fn unconfirmed_echo_is_dropped_after_hold() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        r.set_screen_language(ScreenId::Right, ScreenLanguage::Fixed(Lang::En));
        let t0 = Instant::now();
        r.source_delta("We are live now ", t0);
        r.translation_delta(Lang::En, "We are live now ", t0);
        r.tick(t0 + ms(1000));
        assert_eq!(r.take_echo_dropped(), "We are live now ".chars().count() as u64);
        assert_eq!(r.render(ScreenId::Right).stable, "We are live now");
    }

    #[test]
    fn manual_override_beats_detection() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        let now = Instant::now();
        assert_eq!(r.set_override(Some(Lang::Vi), now), Some(Lang::Vi));
        // English words no longer switch back while overridden.
        assert_eq!(r.source_delta("this should not switch anything at all ", now), None);
        assert_eq!(r.spoken(), Lang::Vi);
        assert_eq!(r.screen_lang(ScreenId::Left), Some(Lang::En));
        // Back to auto: detector state decides.
        r.set_override(None, now);
        assert_eq!(r.override_lang(), None);
    }

    #[test]
    fn partials_off_shows_only_completed_sentences() {
        let mut c = cfg();
        c.show_partials = false;
        let mut r = CaptionRouter::new(c, TranslationMode::EnToVi, Lang::En);
        let now = Instant::now();
        r.translation_delta(Lang::Vi, "Câu một. Câu ha", now);
        assert_eq!(r.render(ScreenId::Left).stable, "Câu một.");
        r.tick(now + ms(5000));
        assert_eq!(r.render(ScreenId::Left).stable, "Câu một. Câu ha");
    }

    #[test]
    fn replace_mode_shows_current_sentence_only() {
        let mut c = cfg();
        c.final_mode = FinalMode::Replace;
        let mut r = CaptionRouter::new(c, TranslationMode::EnToVi, Lang::En);
        let now = Instant::now();
        r.translation_delta(Lang::Vi, "Câu một. Câu hai", now);
        assert_eq!(r.render(ScreenId::Left).stable, "Câu hai");
        r.tick(now + ms(1500));
        assert_eq!(r.render(ScreenId::Left).stable, "Câu hai");
    }

    #[test]
    fn hold_expires_captions() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::EnToVi, Lang::En);
        let t0 = Instant::now();
        r.translation_delta(Lang::Vi, "Xin cảm ơn.", t0);
        assert!(!r.render(ScreenId::Left).stable.is_empty());
        r.tick(t0 + ms(1500)); // idle commit
        r.tick(t0 + ms(5000));
        assert_eq!(r.render(ScreenId::Left).stable, "Xin cảm ơn.");
        r.tick(t0 + ms(9000)); // hold (7 s after the last change) expired
        assert_eq!(r.render(ScreenId::Left).stable, "");
        // New speech appears normally afterwards.
        r.translation_delta(Lang::Vi, "Tiếp tục", t0 + ms(9500));
        assert_eq!(r.render(ScreenId::Left).stable, "Tiếp tục");
    }

    #[test]
    fn clear_hide_freeze_controls() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::EnToVi, Lang::En);
        let now = Instant::now();
        r.translation_delta(Lang::Vi, "Nội dung", now);
        r.control(Control::Hide, now);
        assert!(!r.render(ScreenId::Left).visible);
        r.control(Control::ToggleHidden, now);
        assert!(r.render(ScreenId::Left).visible);
        r.control(Control::Freeze, now);
        assert!(r.frozen);
        r.control(Control::Unfreeze, now);
        r.control(Control::Clear, now);
        assert_eq!(r.render(ScreenId::Left).stable, "");
        r.translation_delta(Lang::Vi, "Mới", now);
        assert_eq!(r.render(ScreenId::Left).stable, "Mới");
    }

    #[test]
    fn soniox_tokens_route_by_language_and_replace_tentative() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::Auto, Lang::En);
        r.set_screen_language(ScreenId::Left, ScreenLanguage::Fixed(Lang::Vi));
        r.set_screen_language(ScreenId::Right, ScreenLanguage::Fixed(Lang::En));
        let now = Instant::now();
        r.ingest_tokens(Stream::Source, Some(Lang::En), "Hello", " every", false, now);
        r.ingest_tokens(Stream::Source, Some(Lang::En), "", " everyone", false, now);
        r.ingest_tokens(Stream::Translation, Some(Lang::Vi), "Xin chào", "", false, now);
        let right = r.render(ScreenId::Right);
        assert_eq!(right.stable, "Hello");
        assert_eq!(right.tentative, " everyone");
        assert_eq!(r.render(ScreenId::Left).stable, "Xin chào");
        r.ingest_tokens(Stream::Source, Some(Lang::En), " everyone", "", true, now);
        assert_eq!(r.render(ScreenId::Right).stable, "Hello everyone");
        let done: Vec<_> = r.take_completed().into_iter().map(|c| (c.kind, c.text)).collect();
        assert!(done.contains(&("original", "Hello everyone".to_string())));
        // Soniox reports Vietnamese speech: screens in fixed mode keep languages.
        r.ingest_tokens(Stream::Source, Some(Lang::Vi), "Cảm ơn", "", false, now);
        assert_eq!(r.spoken(), Lang::Vi);
        assert_eq!(r.render(ScreenId::Left).stable, "Xin chào Cảm ơn");
    }

    #[test]
    fn payload_is_bounded() {
        let mut c = cfg();
        c.max_chars = 50;
        let mut r = CaptionRouter::new(c, TranslationMode::EnToVi, Lang::En);
        let now = Instant::now();
        for i in 0..40 {
            r.translation_delta(Lang::Vi, &format!("câu số {i}. "), now);
        }
        let f = r.render(ScreenId::Left);
        assert!(f.stable.chars().count() <= 50);
        assert!(f.stable.ends_with("câu số 39."));
    }

    #[test]
    fn independent_screen_languages() {
        let mut r = CaptionRouter::new(cfg(), TranslationMode::EnToVi, Lang::En);
        r.set_screen_language(ScreenId::Right, ScreenLanguage::Fixed(Lang::En));
        let now = Instant::now();
        r.source_delta("Hello", now);
        r.translation_delta(Lang::Vi, "Xin chào", now);
        assert_eq!(r.render(ScreenId::Left).stable, "Xin chào");
        assert_eq!(r.render(ScreenId::Right).stable, "Hello");
    }
}
