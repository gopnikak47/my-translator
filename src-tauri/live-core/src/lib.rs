//! Live-event subtitle core for My Translator.
//!
//! Kept free of Tauri/cpal dependencies so the whole caption path — routing,
//! segmentation, OpenAI session supervision and the overlay server — can be
//! tested headless against mock streams.

pub mod audio;
pub mod captions;
pub mod config;
pub mod engine;
pub mod filler;
pub mod hub;
pub mod lang;
pub mod metrics;
pub mod openai;
pub mod overlay;
pub mod router;

pub use config::LiveSettings;
pub use hub::CaptionHub;
pub use lang::Lang;
pub use metrics::Metrics;
