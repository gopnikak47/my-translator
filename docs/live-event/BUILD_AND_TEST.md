# Build, tests and measured results

## Windows build

Prerequisites (as before): Windows 10/11, Rust stable (MSVC toolchain), Node.js
18+, WebView2 runtime, Visual Studio Build Tools (“Desktop development with C++”).

```powershell
git clone https://github.com/<you>/my-translator.git
cd my-translator
git checkout claude/eager-bell-tmflsa
npm install
npm test                      # JS unit tests (Node built-in runner)
cd src-tauri
cargo test -p live-core       # caption engine, OpenAI mock-server and overlay tests
cargo test --lib              # app crate tests
cd ..
npm run build:local           # → src-tauri/target/release/bundle/nsis/*-setup.exe
# (`npm run tauri build` also signs auto-update packages and therefore needs the
#  original author's TAURI_SIGNING_PRIVATE_KEY; `build:local` skips that step.)
```

No new third-party crates or npm packages were added; `live-core` is a local
workspace crate (`src-tauri/live-core`).

Optional checks:

```bash
# Browser test of the overlay in Chromium (needs Playwright installed globally)
(cd src-tauri && cargo build -p live-core --example overlay_demo)
NODE_PATH=$(npm root -g) node tests/browser/overlay.browser.mjs out/

# Demo overlay without any API key (scripted bilingual speech)
cd src-tauri && cargo run -p live-core --example overlay_demo -- 8765 mode-a   # or mode-b
# → open http://127.0.0.1:8765/overlay?screen=left in vMix/OBS/a browser

# Real OpenAI API test (costs money; needs network + a speech WAV, PCM16)
OPENAI_API_KEY=sk-... LIVE_TEST_WAV=speech_en.wav LIVE_TEST_MODE=en_to_vi \
  cargo test -p live-core --test real_api -- --ignored --nocapture
```

## What was run in this environment (Linux container)

| Check | Result |
|---|---|
| `cargo test -p live-core` | **74 passed**, 1 ignored (real API): 60 unit, 7 engine-vs-mock-OpenAI, 7 overlay-server |
| Repeated engine + overlay integration tests ×6 | all passed every run (no flakiness observed) |
| `cargo test --lib` (app crate) | **1 passed** (settings backward compatibility) |
| `npm test` | **8 passed** (Soniox token mapping, OpenAI client sentence finalisation) |
| `cargo clippy` (live-core lib, new app code) | clean |
| `cargo check --target x86_64-pc-windows-gnu` (whole app, incl. WASAPI + Windows memory API) | compiles |
| Full app build + launch on Linux under Xvfb | app starts, overlay server auto-starts on :8765, `/overlay` 200, foreign `Host` 403, settings file + control token created |
| Browser test (headless Chromium 141, 4 overlay pages) | **12/12 checks passed**: transparent page, live updates w/o reloads, exactly 2 lines, canvas regions, hide via control API, token enforcement, auto-reconnect after app restart |
| Event console UI in browser dev mode (mock backend) | renders, mode buttons and settings save work, no JS errors |

Test coverage map (requirement → test):

| Requirement | Tests |
|---|---|
| EN→VI / VI→EN | `engine_mock::en_to_vi_streams_partial_captions`, `::vi_to_en`, `router::en_to_vi_*`, `router::vi_to_en_shows_english` |
| Language switching | `engine_mock::auto_dual_session_follows_language_switch`, `router::auto_mode_switches_language_and_screens_follow`, `router::vi_to_en_switch_routes_first_english_words_correctly`, `lang::*` |
| Manual override | `router::manual_override_beats_detection`, `overlay_server::control_requires_token` (lang-vi) |
| Partial merging / final processing | `captions::deltas_concatenate_without_extra_spaces`, `::sentence_completes_only_when_followed_by_space`, `::done_event_*`, `router::partials_off_*`, JS `openai-realtime-client.test.js` |
| Duplicate suppression | `captions::done_event_matching_stream_is_not_duplicated`, `router::mode_b_fixed_screens_show_original_for_same_language`, `router::echo_arriving_before_switch_is_retracted`, `router::old_echo_is_not_released_by_a_quick_switch`, `engine_mock::reconnects_after_drop_without_duplicate_sessions` |
| Subtitle ordering | `captions::multiple_sentences_in_one_delta_keep_order`, `engine_mock::rotation_before_expiry_keeps_order_without_duplicates`, overlay `seq` guard (browser test) |
| Provider reconnection | `engine_mock::reconnects_after_drop_without_duplicate_sessions`, `::rotation_before_expiry_*` |
| Invalid API credentials | `engine_mock::invalid_api_key_is_fatal_and_not_retried` |
| Microphone disconnection | silence bridging + stall detection in `capture.rs`/`microphone.rs` — **not exercised** (no audio hardware in the container); onset/level logic unit-tested in `audio::*` |
| Overlay client reconnection | `overlay_server::pushes_updates_to_multiple_clients_and_restores_on_reconnect`, browser test “reconnected after app restart” |
| Independent layouts | `router::independent_screen_languages`, `hub::layout_updates_only_when_changed`, browser canvas/both screenshots |
| Emergency hide/show/freeze/clear | `router::clear_hide_freeze_controls`, `hub::freeze_holds_frame_but_hide_still_applies`, browser hide test |
| Soniox fallback | `router::soniox_tokens_route_by_language_and_replace_tentative`, JS `live-soniox-map.test.js` (live Soniox API not exercised) |
| Local server security | `overlay_server::rejects_foreign_host_and_origin`, `::control_requires_token`, `::lan_mode_requires_token_everywhere`, `::serves_overlay_page_with_security_headers` |
| Windows build compatibility | `cargo check` for the Windows target (installer not built) |

## Measured numbers (honest scope)

Only **local** stages could be measured here. All numbers come from this Linux
container (software rendering, headless Chromium, 4 overlay pages open at once,
scripted captions). They are not vMix or LED measurements.

| Stage | n | median | P95 | worst |
|---|---|---|---|---|
| Caption frame published → painted in Chromium (overlay page self-report, same clock) | 217 | 11 ms | 18 ms | 174 ms |

Earlier runs of the same test: median 10–12 ms, P95 18–33 ms, worst 160–174 ms.
The worst case occurs once per page at start-up/reconnect (initial layout).

**Not measured (requires live API, network and hardware):** time to first
translated text, translation update interval, provider-aligned latency, network
RTT, end-to-end speech → LED. The app measures these live in *Event → Performance
& health* (median / P95 / worst, with “~” marking estimates); the opt-in
`real_api` test prints the same statistics. Speech-to-LED latency still needs an
external A/V measurement (e.g. film the speaker and the LED wall at high frame rate).
