# Live-event subtitles (EN ↔ VI) — architecture & engineering notes

This document covers the **🎬 Event** mode added to My Translator: low-latency
English ↔ Vietnamese subtitles pushed to vMix / OBS as transparent browser
sources for side LED screens.

* Operator / production setup → [EVENT_SETUP.md](EVENT_SETUP.md)
* Build, tests and measured results → [BUILD_AND_TEST.md](BUILD_AND_TEST.md)

---

## 1. What existed before (inventory)

| Area | Before this change |
|---|---|
| App | Tauri 2 (Rust backend + plain-JS webview), Windows + macOS |
| Engines | Soniox (WebSocket in the webview), OpenAI `gpt-realtime-translate` (WebSocket in Rust, events forwarded to JS), Qwen LiveTranslate (Rust), Local MLX (macOS only) |
| Audio | `cpal` default microphone **only** (no device choice), WASAPI loopback / ScreenCaptureKit system audio. Every callback resampled to 16 kHz with linear interpolation, then batched for 200 ms and sent to JS as a **JSON array of numbers** |
| OpenAI audio path | Rust → JS (JSON numbers) → `invoke` with `Array.from(bytes)` (JSON again) → Rust → second resample 16 → 24 kHz → WebSocket |
| Display | One always-on-top overlay window (transcript UI). No HTTP server, no vMix/OBS integration |
| Settings | `settings.json` in the user config dir (plain JSON, written in place) |

### Bottlenecks and defects found

1. **OpenAI protocol mismatch.** The official spec (`openai/openai-openapi`,
   `RealtimeTranslation*` schemas) defines only append-only
   `session.input_transcript.delta` / `session.output_transcript.delta`
   events — **no `*.done` events and no language-detection event**. The client
   waited for `session.output_transcript.done` to finalise segments, so
   captions only finalised on Stop.
2. **Audio hops and encodings.** Each 200 ms batch crossed the IPC bridge twice
   as JSON number arrays (~5× payload) before reaching the OpenAI socket.
3. **Double, lossy resampling.** Mic 48 kHz → 16 kHz linear interpolation with no
   anti-alias filter, independently per callback (fractional samples dropped at
   every callback boundary → clicks/drift), then 16 → 24 kHz. Translation quality
   is documented to depend on input audio quality.
4. **200 ms batching on top of** the provider's 200 ms frames.
5. **Single default device, no recovery.** No input selection (USB interface),
   no level meter, no clipping or disconnect detection, no reconnect after a
   device drop.
6. **Captions depended on the webview.** Event handling, timers and rendering
   all lived in the window; background-throttled timers in a minimised
   WebView2 would delay captions.
7. **“Both” audio source interleaves rather than mixes** system + mic chunks
   (pre-existing; not used by event mode, left unchanged — see limitations).

## 2. Architecture

```
 Stage mixer ─► USB audio interface
                     │  (cpal, device chosen by name, own thread, auto-reopen)
                     ▼
            ┌──────────────── Rust (Tauri backend) ───────────────────────────┐
            │ capture.rs  one capture, fan-out ─┬─► 16 kHz PCM16 raw IPC ─► webview (Soniox, Qwen, Local)
            │   band-limited resample           │
            │   device-rate → 24 kHz            └─► 24 kHz 200 ms chunks
            │                                          │  (base64 encoded ONCE, shared)
            │ live-core::engine ── lane →vi ─► wss /v1/realtime/translations
            │                     lane →en ─► wss /v1/realtime/translations (dual mode)
            │        deltas ─► live-core::hub / router (EN & VI tracks, Mode A/B)
            │                     │  watch channels (latest frame only)
            │ live-core::overlay  ▼  HTTP + SSE on 127.0.0.1:8765
            └─────────────────────┬───────────────────────────────────────────┘
                                  ▼
          vMix Web Browser input / OBS Browser Source (transparent page)
              /overlay?screen=left | right | both | canvas
```

* **`src-tauri/live-core/`** — new Tauri-free crate (workspace member) so the
  whole caption path is unit-testable headless:
  * `lang.rs` – EN/VI detection from transcript text (Vietnamese diacritics,
    sliding window with hysteresis; offline, no extra API calls).
  * `captions.rs` – caption track: stable (append-only) text, revisable tentative
    tail, sentence segmentation (abbreviations, decimals, quotes), soft breaks for
    long unpunctuated runs, duplicate-safe handling of legacy `.done` events.
  * `router.rs` – bilingual routing: tracks per display language, display modes
    A/B, echo holding, manual override, hold/expiry, hide/freeze/clear.
  * `hub.rs` – shared caption state; publishes per-screen frames through
    `tokio::sync::watch` (latest value only ⇒ bounded, slow clients skip frames).
  * `openai.rs` / `engine.rs` – OpenAI translation sessions and their supervisor.
  * `overlay.rs` + `assets/overlay.{html,js}` – local HTTP/SSE server and page.
  * `audio.rs` – streaming resampler, 200 ms chunker, onset detector.
  * `metrics.rs` – per-stage latency series (median / P95 / worst).
* **`src-tauri/src/audio/capture.rs`, `microphone.rs`** — single shared capture
  with device selection, supervision, level meter, silence bridging.
* **`src-tauri/src/commands/live.rs`** — Tauri commands for the console.
* **`src/js/event-console.js`** — operator console (🎬 Event tab), Soniox fallback.

The OpenAI live path never touches the webview: if the operator window is
minimised or busy, subtitles keep flowing.

## 3. Bilingual strategy (Mode 3) — decision

The translation API has a fixed output language per session, no source-language
setting and no language-detection event (only optional source transcription with
`gpt-realtime-whisper`). Two approaches are implemented and selectable:

| | **Dual session (default)** | **Single session + switch** |
|---|---|---|
| Sessions | 2 persistent (→vi with transcription, →en) fed by one capture | 1 (with transcription) |
| Switch latency | none — both translations already streaming | detection + `session.update` round trip; speech in flight at the switch comes out in the old language |
| Cost | ~2× translation minutes | ~1× |
| Routing | per-delta: the session whose target ≠ spoken language is shown; the other is an echo and is discarded (held 900 ms first, so early output is not lost) | output follows the current target |

Language detection uses the source transcript (Vietnamese is unambiguous from its
diacritics). Detection lag is hidden by: routing marked Vietnamese fragments
immediately, holding unaccented words for ≤1.5 s while Vietnamese is spoken,
holding/releasing same-language output, and retracting a just-shown echo after a
switch. Operator hotkeys (EN / VI / Auto) override detection at any time.

**Why dual is the default:** for a live event, a wrong-language or late first
sentence after every speaker change is more visible than the API cost difference.
**Measured comparison against the live API was not possible in this environment**
(no API key / no network access to api.openai.com); run the opt-in real-API test
in [BUILD_AND_TEST.md](BUILD_AND_TEST.md) with both strategies to confirm on site.
One-way modes (EN→VI, VI→EN) use a single session and remain the dependable fallback.

## 4. Performance optimisations

| Change | Effect |
|---|---|
| OpenAI audio path entirely in Rust | removes 2 IPC hops and 2 JSON encodings per chunk |
| Device rate → 24 kHz in one band-limited, stateful resampling step | no aliasing, no per-callback discontinuities, no 16 kHz bandwidth cap |
| 200 ms chunks aligned to the API's 200 ms engine frames (configurable 20–400) | follows the documented recommendation; no extra local batching |
| Base64 + JSON built once per chunk and shared (`Utf8Bytes`) across sessions | dual mode costs no extra encoding |
| Continuous audio incl. silence; digital silence bridged when the device stalls | session clock stays continuous and warm (spec: keep appending silence) |
| Captions rendered from partial deltas immediately; segmentation is local | no waiting for sentence end; nothing waits on `.done` |
| `watch` channels per screen + optional throttle | coalescing, bounded memory, no backlog for slow clients |
| SSE push (no polling), one `requestAnimationFrame` per update, text nodes touched only on change, no animations | measured overlay paint latency: median ≈ 11 ms (see test report) |
| Legacy webview engines: raw binary IPC (`ArrayBuffer`), 100 ms batches | Soniox/Qwen/Local get ~5× smaller payloads and half the batching delay |
| Legacy OpenAI desktop mode: raw binary `invoke` + sentence finalisation | fixes never-finalised captions, removes JSON number arrays |

## 5. Reliability

* Reconnect with exponential backoff (300 ms → 10 s cap, jitter); backoff resets
  after 30 s healthy. At most one active session per lane — no duplicates.
* Authentication / quota / model-access errors stop retries immediately and
  show a plain-language operator message (never on the LED).
* WebSocket ping every 10 s; no data or pong for 30 s ⇒ reconnect. RTT measured.
* Session expiry (`expires_at` from `session.created`): a standby session opens
  90 s before expiry; audio switches at the next pause (or 15 s before expiry);
  the old session gets `session.close` and flushes; the new session's output is
  held until the old one finished ⇒ ordered, no duplicates.
* Bounded queues everywhere; a session that cannot keep up for 3 s is replaced
  rather than accumulating latency. Audio callback never blocks.
* Audio device errors / stalls ⇒ stream reopened with backoff (USB unplug/replug).
* Graceful stop/exit: `session.close` to every session, overlay server stopped,
  capture threads joined. Settings written atomically (temp file + rename).
* Overlay page: reconnects by itself (including when Chromium's `EventSource`
  gives up during an app restart), never reloads, clears stale captions if the
  app disappears.

## 6. Security

* Overlay server binds **127.0.0.1** by default. `Host` header is validated
  (DNS-rebinding protection) and cross-origin `Origin`s are rejected; no CORS
  headers; strict CSP on the page; 64-connection cap, header/body limits, timeouts.
* Overlay frames contain caption text only. API keys never leave the backend;
  the OpenAI key is only sent to `api.openai.com` in the `Authorization` header.
* Control API (`POST /control/<action>`) always requires the per-install random
  token (`X-Access-Token` header or `token=`). LAN mode (off by default) requires
  the token on every request.

## 7. Soniox fallback

* Provider selector + **“Fall back to Soniox” / “Back to OpenAI”** button.
* Switching stops the running provider completely before starting the other one
  (never both ⇒ no duplicate captions, no double billing). Overlay URLs do not
  change; the screens keep the last caption until the new provider speaks.
* Soniox runs in the webview with the existing client (now exposing raw tokens);
  its final/non-final tokens and language tags feed the same caption hub.
  Long-event retries continue with capped backoff unless the error is fatal.
* **No automatic provider fallback** was implemented: switching mid-sentence
  automatically could duplicate or drop text and double API spend. The console
  shows a clear alert when OpenAI is reconnecting so the operator can decide.

## 8. Files

New: `src-tauri/live-core/**` (crate, assets, tests, examples),
`src-tauri/src/audio/capture.rs`, `src-tauri/src/audio/unsupported.rs`,
`src-tauri/src/commands/live.rs`, `src-tauri/src/sysinfo.rs`,
`src/js/event-console.js`, `src/js/live-soniox-map.js`, `src/styles/event.css`,
`tests/js/*.test.js`, `tests/browser/overlay.browser.mjs`, `docs/live-event/*`.

Modified: `src-tauri/Cargo.toml` (workspace + `live-core` path dep + 2 Windows
API features), `src-tauri/Cargo.lock` (local crate only), `src-tauri/src/lib.rs`,
`src-tauri/src/settings.rs`, `src-tauri/src/audio/{mod,microphone,wasapi,system_audio}.rs`,
`src-tauri/src/commands/{mod,audio,settings,openai_realtime}.rs`, `src/index.html`,
`src/js/{app,ui-shell,soniox,openai-realtime-client,tauri-mock}.js`,
`package.json` (test scripts), `README.md`.

No new third-party crates or npm packages were added.

## 9. Known issues & limitations

* **Not tested against the live OpenAI or Soniox APIs** in this environment (no
  credentials, no network route). Protocol behaviour was verified against the
  official OpenAPI spec and a protocol-faithful mock server.
* **No physical vMix / OBS / LED test** was performed. The overlay was tested in
  headless Chromium (the engine family both products embed).
* Windows: the code type-checks for the Windows target (`cargo check
  --target x86_64-pc-windows-gnu`), but the MSVC installer was **not** built here.
* Semantics of `elapsed_ms` are not fully specified by OpenAI; the
  “audio position → text” metric assumes it is the input-audio position and is
  labelled *estimated*.
* Language detection needs the source transcript (`gpt-realtime-whisper`). If
  it is unavailable, Auto mode shows the opposite of the assumed start language;
  use the EN/VI hotkeys.
* Behaviour of the →en session on English input (and →vi on Vietnamese) is not
  documented; the router treats it as an echo and shows the source transcript
  instead.
* Hotkeys (Ctrl+Alt+H/F/C/A/E/V) work while the app window has focus; for
  control from vMix/Stream Deck use the HTTP control API.
* Pre-existing: “Both” audio source interleaves system + mic chunks rather than
  mixing them (unchanged; event mode always uses one input).
* API keys remain stored in plain JSON in the user profile (pre-existing design);
  they are never served over HTTP.

## 10. Rollback

* Operationally: just don't use the 🎬 Event tab — all previous engines and the
  overlay window work as before (the shared capture keeps the same commands).
  Set `live.server.auto_start=false` (Event tab → “Start with the app”) to keep
  port 8765 closed.
* Code: the change is isolated on branch `claude/eager-bell-tmflsa`.
  `git revert <merge-commit>` (or check out commit `3495f99`, “chore: release
  v0.9.1”) restores the previous version. Settings stay compatible: older
  versions ignore the extra `live` block in `settings.json` (and drop it the
  next time they save, so Event-tab settings return to defaults).
