/**
 * Mock Tauri APIs for browser-only UI testing (dev server on :3111).
 * Loaded before app.js. Dev-only: activates ONLY when the real Tauri runtime is
 * absent AND we are on the dev port — so it can never shadow window.__TAURI__ in the
 * packaged app (production origin is tauri://localhost or http://tauri.localhost, no :3111).
 */

if (!window.__TAURI__ && location.port === '3111') {
    console.warn('[tauri-mock] Browser dev mode — Tauri APIs are mocked');

    const mockSettings = {
        translation_mode: 'soniox',
        source_language: 'auto',
        target_language: 'vi',
        soniox_api_key: '',
        elevenlabs_api_key: '',
        google_tts_api_key: '',
        openai_api_key: '',
        qwen_api_key: '',
        tts_provider: 'edge',
        edge_tts_voice: 'vi-VN-HoaiMyNeural',
        edge_tts_speed: 50,
        google_tts_voice: 'vi-VN-Chirp3-HD-Aoede',
        google_tts_speed: 1.0,
        // New free online providers (Trudio-style)
        microsoft_v2_voice: 'vi-VN-HoaiMyNeural',
        microsoft_v2_speed: 20,
        google_free_voice: 'vi-VN',
        google_free_api_key: '',
        google_free_speed: 1.0,
        tiktok_voice: 'BV074_streaming',
        tiktok_speed: 1.0,
        tiktok_session_id: '',
        local_tts_voice: 'vi_VN-vais1000-medium',
        local_tts_speed: 1.0,
        local_tts_models_dir: '',
        font_size: 16,
        // Live-event subtitles (mirrors live_core::LiveSettings::default()).
        live: {"provider": "openai", "mode": "auto", "strategy": "dual_session", "default_spoken": "en", "input_device": "", "openai_chunk_ms": 200, "noise_reduction": "near_field", "captions": {"show_partials": true, "throttle_ms": 0, "hold_ms": 7000, "idle_commit_ms": 1200, "max_segment_chars": 140, "max_chars": 360, "final_mode": "rollup", "tentative_style": "dim", "echo_hold_ms": 900, "filter_fillers": true, "filler_words": ["ừm", "ờm", "ưm", "hừm", "ờ", "à", "um", "uhm", "uh", "er", "erm", "ah", "oh", "hmm"]}, "left": {"language": "opposite", "ref_width": 960, "ref_height": 1080, "font_family": "\"Segoe UI\", Arial, \"Noto Sans\", \"Roboto\", sans-serif", "font_size": 64, "font_weight": 700, "line_height": 1.25, "color": "#ffffff", "outline_color": "#000000", "outline_px": 3.0, "shadow": true, "backing": false, "backing_color": "#000000", "backing_opacity": 0.55, "margin_x": 48, "margin_y": 64, "valign": "bottom", "align": "center", "max_lines": 2}, "right": {"language": "opposite", "ref_width": 960, "ref_height": 1080, "font_family": "\"Segoe UI\", Arial, \"Noto Sans\", \"Roboto\", sans-serif", "font_size": 64, "font_weight": 700, "line_height": 1.25, "color": "#ffffff", "outline_color": "#000000", "outline_px": 3.0, "shadow": true, "backing": false, "backing_color": "#000000", "backing_opacity": 0.55, "margin_x": 48, "margin_y": 64, "valign": "bottom", "align": "center", "max_lines": 2}, "canvas": {"width": 3840, "height": 1080, "left": {"x": 0, "y": 0, "width": 960, "height": 1080}, "center": {"x": 960, "y": 0, "width": 1920, "height": 1080}, "right": {"x": 2880, "y": 0, "width": 960, "height": 1080}, "guides": false}, "server": {"port": 8765, "allow_lan": false, "access_token": "mock-token-0123456789", "auto_start": true}},
    };

    // Live-event console sample status (browser dev only).
    const mockLiveStatus = () => ({
        provider: null,
        uptime_s: 0,
        engine: null,
        router: { mode: 'auto', spoken: 'en', detected: 'en', override_lang: null, hidden: false, frozen: false, screen_langs: ['vi', 'vi'] },
        audio: { state: 'running', error: null, device: 'Focusrite USB (Scarlett 2i2)', sample_rate: 48000, peak: 0.35 + Math.random() * 0.2, rms: 0.08 + Math.random() * 0.05, clipped_total: 0, clipping_recent: false, ms_since_audio: 12, frames_dropped: 0, device_reconnects: 0, silence_filled_ms: 0, stream_errors: 0, sample_format: 'F32', channels: 2, peak_left: 0.3 + Math.random() * 0.1, peak_right: 0.0008, gain_db: 0, channel_mode: 'auto', i32_right_aligned: false },
        metrics: { series: {}, counters: {}, overlay_clients: 2 },
        server: { running: true, port: 8765, allow_lan: false, error: null, control_token: 'mock-token-0123456789',
            urls: ['left', 'right', 'both', 'canvas'].map((s) => [s, `http://127.0.0.1:8765/overlay?screen=${s}`]) },
        memory_mb: 142,
    });

    // Sample local (Piper) voice catalog so the download/delete UI can be exercised.
    const mockLocalVoices = [
        { id: 'vi_VN-vais1000-medium', display: 'Tiếng Việt — VAIS1000 (medium)', lang: 'vi', url: '', approxSizeBytes: 67154040, sampleRate: 22050, installed: true, installedBytes: 67154040, imported: false },
        { id: 'vi_VN-25hours_single-low', display: 'Tiếng Việt — 25 hours (low)', lang: 'vi', url: '', approxSizeBytes: 67059380, sampleRate: 16000, installed: false, installedBytes: null, imported: false },
        { id: 'en_US-ryan-medium', display: 'English (US) — Ryan (medium)', lang: 'en', url: '', approxSizeBytes: 63000000, sampleRate: 22050, installed: true, installedBytes: 63000000, imported: false },
        { id: 'en_US-lessac-medium', display: 'English (US) — Lessac (medium)', lang: 'en', url: '', approxSizeBytes: 67230653, sampleRate: 22050, installed: false, installedBytes: null, imported: false },
        // Sample imported (local) voices — always shown regardless of language filter.
        { id: 'tranthanh3870', display: 'tranthanh3870', lang: '', url: '', approxSizeBytes: 0, sampleRate: 0, installed: true, installedBytes: 63000000, imported: true },
        { id: 'mytam2', display: 'mytam2', lang: '', url: '', approxSizeBytes: 0, sampleRate: 0, installed: true, installedBytes: 63000000, imported: true },
    ];

    // Tiny decodable silent WAV (8kHz mono, ~60ms) so Read mode's reader→player loop can
    // advance in browser dev (empty base64 would fail decodeAudioData → every chunk errors).
    const mockSilentWav = 'UklGRuQDAABXQVZFZm10IBAAAAABAAEAQB8AAIA+AAACABAAZGF0YcADAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=';

    // Sample voices so the Microsoft v2 dynamic-populate path can be exercised in-browser.
    const mockMsVoices = JSON.stringify([
        { short_name: 'vi-VN-HoaiMyNeural', friendly_name: 'HoaiMy', gender: 'Female', locale: 'vi-VN' },
        { short_name: 'vi-VN-NamMinhNeural', friendly_name: 'NamMinh', gender: 'Male', locale: 'vi-VN' },
        { short_name: 'en-US-JennyNeural', friendly_name: 'Jenny', gender: 'Female', locale: 'en-US' },
        { short_name: 'en-US-GuyNeural', friendly_name: 'Guy', gender: 'Male', locale: 'en-US' },
        { short_name: 'en-GB-SoniaNeural', friendly_name: 'Sonia', gender: 'Female', locale: 'en-GB' },
    ]);

    window.__TAURI__ = {
        core: {
            invoke: async (cmd, args) => {
                console.log('[tauri-mock] invoke:', cmd, args);
                switch (cmd) {
                    case 'get_settings':
                        return mockSettings;
                    case 'save_settings':
                        Object.assign(mockSettings, args?.newSettings || {});
                        return null;
                    case 'get_platform_info':
                        return { os: 'macos', arch: 'aarch64' };
                    case 'microsoft_list_voices':
                        return mockMsVoices;
                    case 'local_tts_list_models':
                        return mockLocalVoices;
                    case 'local_tts_models_dir_path':
                        return mockSettings.local_tts_models_dir ||
                            '/Users/dev/Library/Application Support/com.personal.translator/piper-models';
                    case 'local_tts_download_model': {
                        // Simulate progress then mark installed.
                        const ch = args?.onProgress;
                        const id = args?.id;
                        if (ch && typeof ch.onmessage === 'function') {
                            [25, 50, 75, 100].forEach((pct) =>
                                ch.onmessage({ id, phase: 'downloading', received: pct, total: 100, message: null })
                            );
                            ch.onmessage({ id, phase: 'done', received: 100, total: 100, message: null });
                        }
                        const v = mockLocalVoices.find((m) => m.id === id);
                        if (v) v.installed = true;
                        return null;
                    }
                    case 'local_tts_delete_model': {
                        const v = mockLocalVoices.find((m) => m.id === args?.id);
                        if (v) v.installed = false;
                        return null;
                    }
                    // TTS synth commands: return a tiny decodable silent WAV so Read mode's
                    // reader→player loop can advance in browser dev (real audio only in Tauri).
                    case 'edge_tts_speak':
                    case 'google_free_tts_speak':
                    case 'tiktok_tts_speak':
                    case 'local_tts_speak':
                        return mockSilentWav;
                    case 'start_capture':
                    case 'stop_capture':
                    case 'start_mic_capture':
                    case 'stop_mic_capture':
                        return null;
                    case 'list_transcripts':
                        return [];
                    case 'audio_list_input_devices':
                        return [
                            { name: 'Microphone (Realtek Audio)', is_default: true, channels: 2, sample_rate: 48000 },
                            { name: 'Focusrite USB (Scarlett 2i2)', is_default: false, channels: 2, sample_rate: 48000 },
                        ];
                    case 'audio_set_input_options':
                        return null;
                    case 'audio_status':
                        return mockLiveStatus().audio;
                    case 'live_status':
                        return mockLiveStatus();
                    case 'live_start':
                    case 'live_stop':
                    case 'live_control':
                    case 'live_soniox_ingest':
                    case 'live_subscribe_segments':
                    case 'live_server_restart':
                    case 'live_server_stop':
                    case 'live_reset_metrics':
                        return null;
                    case 'check_update':
                        return null;
                    default:
                        console.warn('[tauri-mock] unhandled command:', cmd);
                        return null;
                }
            },
            Channel: class {
                constructor() { this.onmessage = null; }
            },
        },
        dialog: {
            // Browser dev: pretend the user picked a folder (null would mean "cancelled").
            open: async () => '/Users/dev/piper-models',
        },
        window: {
            getCurrentWindow: () => ({
                setAlwaysOnTop: async () => {},
                outerPosition: async () => ({ x: 100, y: 100 }),
                innerSize: async () => ({ width: 800, height: 600 }),
                setPosition: async () => {},
                setSize: async () => {},
                close: async () => { window.close(); },
                minimize: async () => {},
                toggleMaximize: async () => {},
                onCloseRequested: async () => () => {},
                listen: async () => () => {},
            }),
        },
    };
}
