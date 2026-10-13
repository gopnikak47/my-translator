//! Persisted live-event configuration. Every struct uses `#[serde(default)]` so
//! older settings files (without these fields) keep loading unchanged.

use crate::lang::Lang;
use serde::{Deserialize, Serialize};

/// Which spoken language(s) to expect and which direction to translate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationMode {
    /// Mode 1: listen to English, produce Vietnamese.
    EnToVi,
    /// Mode 2: listen to Vietnamese, produce English.
    ViToEn,
    /// Mode 3: detect spoken language, show the opposite language.
    Auto,
}

impl TranslationMode {
    /// Fixed spoken language for one-way modes.
    pub fn fixed_source(self) -> Option<Lang> {
        match self {
            TranslationMode::EnToVi => Some(Lang::En),
            TranslationMode::ViToEn => Some(Lang::Vi),
            TranslationMode::Auto => None,
        }
    }
}

/// How bilingual (Auto) mode is realised with the OpenAI translation API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BilingualStrategy {
    /// Two persistent, prewarmed sessions (→vi and →en) fed by one capture.
    /// Zero switch latency, ~2× translation cost.
    DualSession,
    /// One session whose `audio.output.language` is changed with
    /// `session.update` when the spoken language flips. ~1× cost, but speech
    /// already in flight at the switch is translated into the old language.
    SingleSwitch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Openai,
    Soniox,
}

/// What a subtitle screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenLanguage {
    /// Display mode A: always the language opposite to the one being spoken.
    Opposite,
    /// Display mode B: a fixed language. When the speaker already uses this
    /// language the original transcript is shown instead of a translation.
    Fixed(Lang),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalMode {
    /// Roll-up: completed sentences stay visible until pushed out by new lines.
    Rollup,
    /// Pop-on: only the current (or last completed) sentence is shown.
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TentativeStyle {
    /// Revisable text looks identical to stable text.
    Same,
    /// Revisable text is rendered at reduced opacity.
    Dim,
    /// Revisable text is rendered in italics.
    Italic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptionConfig {
    /// Show text of the sentence still being spoken (lowest latency).
    pub show_partials: bool,
    /// Minimum interval between overlay pushes per client (0 = immediate).
    pub throttle_ms: u64,
    /// Remove captions after this long without new text (0 = never).
    pub hold_ms: u64,
    /// Finalise the in-progress sentence after this long without new text.
    pub idle_commit_ms: u64,
    /// Force a segment break when an unpunctuated sentence gets this long.
    pub max_segment_chars: usize,
    /// Upper bound of characters sent to a screen (the overlay then clips to
    /// `max_lines` visually, so this only bounds payload size).
    pub max_chars: usize,
    pub final_mode: FinalMode,
    pub tentative_style: TentativeStyle,
    /// How long translated text for a language equal to the detected spoken
    /// language is held before being discarded as an echo (bilingual routing).
    pub echo_hold_ms: u64,
    /// Remove filler words (ừm, à, oh, uh…) from displayed captions.
    pub filter_fillers: bool,
    /// Single words treated as fillers (case-insensitive, stretched forms match).
    pub filler_words: Vec<String>,
}

impl Default for CaptionConfig {
    fn default() -> Self {
        Self {
            show_partials: true,
            throttle_ms: 0,
            hold_ms: 7000,
            idle_commit_ms: 1200,
            max_segment_chars: 140,
            max_chars: 360,
            final_mode: FinalMode::Rollup,
            tentative_style: TentativeStyle::Dim,
            echo_hold_ms: 900,
            filter_fillers: true,
            filler_words: crate::filler::DEFAULT_FILLERS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Visual style of one subtitle screen (left or right LED).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenStyle {
    pub language: ScreenLanguage,
    /// Reference resolution the sizes below are expressed in. The overlay
    /// scales everything to the actual browser-source size.
    pub ref_width: u32,
    pub ref_height: u32,
    pub font_family: String,
    pub font_size: u32,
    pub font_weight: u32,
    pub line_height: f32,
    pub color: String,
    pub outline_color: String,
    pub outline_px: f32,
    pub shadow: bool,
    pub backing: bool,
    pub backing_color: String,
    pub backing_opacity: f32,
    pub margin_x: u32,
    pub margin_y: u32,
    pub valign: VAlign,
    pub align: HAlign,
    pub max_lines: u32,
}

impl Default for ScreenStyle {
    fn default() -> Self {
        Self {
            language: ScreenLanguage::Opposite,
            ref_width: 960,
            ref_height: 1080,
            // System fonts with full Vietnamese coverage on Windows first.
            font_family: "\"Segoe UI\", Arial, \"Noto Sans\", \"Roboto\", sans-serif".into(),
            font_size: 64,
            font_weight: 700,
            line_height: 1.25,
            color: "#ffffff".into(),
            outline_color: "#000000".into(),
            outline_px: 3.0,
            shadow: true,
            backing: false,
            backing_color: "#000000".into(),
            backing_opacity: 0.55,
            margin_x: 48,
            margin_y: 64,
            valign: VAlign::Bottom,
            align: HAlign::Center,
            max_lines: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Configuration A: one wide canvas whose regions an LED processor maps to the
/// physical screens. Coordinates are examples only and fully editable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CanvasConfig {
    pub width: u32,
    pub height: u32,
    pub left: Region,
    pub center: Region,
    pub right: Region,
    /// Draw region outlines in the canvas page (alignment aid; never on air).
    pub guides: bool,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            width: 3840,
            height: 1080,
            left: Region { x: 0, y: 0, width: 960, height: 1080 },
            center: Region { x: 960, y: 0, width: 1920, height: 1080 },
            right: Region { x: 2880, y: 0, width: 960, height: 1080 },
            guides: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayServerConfig {
    pub port: u16,
    /// Bind 0.0.0.0 instead of 127.0.0.1 (only when vMix/OBS runs on another
    /// machine). Requires `access_token` on every request when enabled.
    pub allow_lan: bool,
    /// Random token for the control API (and for every request in LAN mode).
    pub access_token: String,
    pub auto_start: bool,
}

impl Default for OverlayServerConfig {
    fn default() -> Self {
        Self {
            port: 8765,
            allow_lan: false,
            access_token: String::new(),
            auto_start: true,
        }
    }
}

/// Everything the overlay pages need to draw (sent on connect + on change).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayLayout {
    pub left: ScreenStyle,
    pub right: ScreenStyle,
    pub canvas: CanvasConfig,
    pub tentative_style: TentativeStyle,
    pub hold_ms: u64,
}

impl Default for OverlayLayout {
    fn default() -> Self {
        let left = ScreenStyle::default();
        let right = ScreenStyle::default();
        Self {
            left,
            right,
            canvas: CanvasConfig::default(),
            tentative_style: TentativeStyle::Dim,
            hold_ms: CaptionConfig::default().hold_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveSettings {
    pub provider: Provider,
    pub mode: TranslationMode,
    pub strategy: BilingualStrategy,
    /// Spoken language assumed at start of Auto mode before detection.
    pub default_spoken: Lang,
    /// Input device name ("" = system default input).
    pub input_device: String,
    /// Audio chunk length sent to OpenAI. The API consumes 200 ms frames and
    /// recommends 200 ms appends.
    pub openai_chunk_ms: u32,
    /// "near_field" | "far_field" | "" (off).
    pub noise_reduction: String,
    pub captions: CaptionConfig,
    pub left: ScreenStyle,
    pub right: ScreenStyle,
    pub canvas: CanvasConfig,
    pub server: OverlayServerConfig,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            provider: Provider::Openai,
            mode: TranslationMode::Auto,
            strategy: BilingualStrategy::DualSession,
            default_spoken: Lang::En,
            input_device: String::new(),
            openai_chunk_ms: 200,
            noise_reduction: "near_field".into(),
            captions: CaptionConfig::default(),
            left: ScreenStyle::default(),
            right: ScreenStyle::default(),
            canvas: CanvasConfig::default(),
            server: OverlayServerConfig::default(),
        }
    }
}

impl LiveSettings {
    pub fn layout(&self) -> OverlayLayout {
        OverlayLayout {
            left: self.left.clone(),
            right: self.right.clone(),
            canvas: self.canvas.clone(),
            tentative_style: self.captions.tentative_style,
            hold_ms: self.captions.hold_ms,
        }
    }

    /// Clamp user-entered values into safe ranges.
    pub fn sanitized(mut self) -> Self {
        let c = &mut self.captions;
        c.throttle_ms = c.throttle_ms.min(2000);
        c.hold_ms = c.hold_ms.min(600_000);
        c.idle_commit_ms = c.idle_commit_ms.clamp(200, 10_000);
        c.max_segment_chars = c.max_segment_chars.clamp(30, 1000);
        c.max_chars = c.max_chars.clamp(40, 4000);
        c.echo_hold_ms = c.echo_hold_ms.min(5000);
        c.filler_words = c
            .filler_words
            .iter()
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty() && w.chars().count() <= 32)
            .take(200)
            .collect();
        self.openai_chunk_ms = self.openai_chunk_ms.clamp(20, 400);
        for s in [&mut self.left, &mut self.right] {
            s.max_lines = s.max_lines.clamp(1, 10);
            s.font_size = s.font_size.clamp(8, 400);
            s.font_weight = s.font_weight.clamp(100, 900);
            s.line_height = s.line_height.clamp(0.8, 3.0);
            s.outline_px = s.outline_px.clamp(0.0, 20.0);
            s.backing_opacity = s.backing_opacity.clamp(0.0, 1.0);
            s.ref_width = s.ref_width.clamp(16, 16384);
            s.ref_height = s.ref_height.clamp(16, 16384);
        }
        if self.server.port < 1024 {
            self.server.port = 8765;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_gives_defaults() {
        let s: LiveSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, LiveSettings::default());
        assert_eq!(s.left.max_lines, 2);
        assert_eq!(s.server.port, 8765);
    }

    #[test]
    fn partial_json_keeps_other_defaults() {
        let s: LiveSettings =
            serde_json::from_str(r#"{"mode":"en_to_vi","left":{"font_size":80,"language":{"fixed":"vi"}}}"#)
                .unwrap();
        assert_eq!(s.mode, TranslationMode::EnToVi);
        assert_eq!(s.left.font_size, 80);
        assert_eq!(s.left.language, ScreenLanguage::Fixed(Lang::Vi));
        assert_eq!(s.left.max_lines, 2);
        assert_eq!(s.right.language, ScreenLanguage::Opposite);
    }

    #[test]
    fn sanitize_clamps() {
        let mut s = LiveSettings::default();
        s.left.max_lines = 0;
        s.server.port = 80;
        s.openai_chunk_ms = 5;
        let s = s.sanitized();
        assert_eq!(s.left.max_lines, 1);
        assert_eq!(s.server.port, 8765);
        assert_eq!(s.openai_chunk_ms, 20);
    }
}
