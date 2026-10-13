//! Overlay demo / browser test harness.
//!
//! Starts the real caption hub + overlay server and streams a scripted bilingual
//! "speech" through the same router the OpenAI engine uses (source transcript
//! deltas + translated deltas, word by word). Use it to check vMix / OBS
//! browser sources without an API key:
//!
//!   cargo run --example overlay_demo -- [port] [mode-a|mode-b]
//!
//! then open http://127.0.0.1:<port>/overlay?screen=left (or right/both/canvas).

use live_core::config::{LiveSettings, ScreenLanguage};
use live_core::overlay::{hub_control_handler, OverlayServer, ServerOptions};
use live_core::{CaptionHub, Lang, Metrics};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCRIPT: &[(Lang, &str, &str)] = &[
    (
        Lang::En,
        "Good evening ladies and gentlemen, and welcome to the annual technology summit. Tonight we celebrate twenty years of innovation.",
        "Xin chào quý vị và các bạn, chào mừng đến với hội nghị công nghệ thường niên. Tối nay chúng ta kỷ niệm hai mươi năm đổi mới.",
    ),
    (
        Lang::Vi,
        "Xin trân trọng cảm ơn sự có mặt của quý vị đại biểu. Sau đây là phần phát biểu của ban tổ chức.",
        "Thank you very much to all distinguished guests for being here. Next is a speech from the organizing committee.",
    ),
];

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(8765);
    let mode_b = args.get(2).map(|m| m == "mode-b").unwrap_or(false);

    let mut settings = LiveSettings::default();
    if mode_b {
        settings.left.language = ScreenLanguage::Fixed(Lang::Vi);
        settings.right.language = ScreenLanguage::Fixed(Lang::En);
    }
    settings.left.backing = true;
    let metrics = Arc::new(Metrics::new());
    let hub = CaptionHub::new(&settings, metrics.clone());
    tokio::spawn(CaptionHub::run_ticker(Arc::downgrade(&hub)));
    let server = OverlayServer::start(
        ServerOptions { port, allow_lan: false, token: "demo-token-0123456789".into() },
        hub.clone(),
        hub_control_handler(hub.clone()),
    )
    .await
    .expect("start overlay server");
    println!("overlay demo on http://{}/overlay?screen=left", server.addr());

    let started = Instant::now();
    loop {
        for (spoken, source, translation) in SCRIPT {
            let src: Vec<&str> = source.split_inclusive(' ').collect();
            let tr: Vec<&str> = translation.split_inclusive(' ').collect();
            let steps = src.len().max(tr.len());
            for i in 0..steps {
                if let Some(w) = src.get(i) {
                    hub.source_delta(w, Instant::now());
                }
                if let Some(w) = tr.get(i) {
                    hub.translation_delta(spoken.opposite(), w, Instant::now());
                }
                tokio::time::sleep(Duration::from_millis(180)).await;
            }
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        if let Some(s) = metrics.summary(live_core::metrics::names::OVERLAY_RENDER) {
            println!(
                "[{:>4}s] overlay paint latency: median {:.0} ms, P95 {:.0} ms, worst {:.0} ms (n={})",
                started.elapsed().as_secs(),
                s.p50_ms,
                s.p95_ms,
                s.max_ms,
                s.count
            );
        }
    }
}
