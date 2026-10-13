/**
 * Event Console — operator panel for live-event broadcast subtitles.
 *
 * Start/stop OpenAI Realtime (runs in Rust) or the Soniox fallback (runs here),
 * pick the audio interface, choose translation/display modes, style each LED
 * screen, copy overlay URLs for vMix/OBS, emergency controls, health/latency.
 *
 * Captions never pass through this window on the OpenAI path, so minimising it
 * does not affect the subtitles on air.
 */

import { settingsManager } from './settings.js';
import { SonioxClient } from './soniox.js';
import { mapSonioxTokens, sonioxConfigForMode } from './live-soniox-map.js';
import { parseFillerList } from './filler-filter.js';

const { invoke, Channel } = window.__TAURI__.core;

const RES_PRESETS = [
    ['960x1080', '960 × 1080 (side LED, half-HD portrait)'],
    ['1080x1920', '1080 × 1920 (portrait)'],
    ['1920x1080', '1920 × 1080 (HD)'],
    ['1280x720', '1280 × 720'],
    ['768x1152', '768 × 1152'],
    ['custom', 'Custom…'],
];

// Settings that only take effect on the next Start.
const RESTART_KEYS = new Set(['mode', 'strategy', 'input_device', 'openai_chunk_ms', 'noise_reduction']);

const METRIC_LABELS = [
    ['audio_capture_ms', 'Audio capture (device → app)'],
    ['audio_buffering_ms', 'Audio buffering (chunk + queue)'],
    ['audio_send_ms', 'Audio send (WebSocket write)'],
    ['network_rtt_ms', 'Network round trip (ping)'],
    ['time_to_first_delta_ms', 'Speech onset → first translated text'],
    ['provider_aligned_latency_ms', 'Audio position → translated text'],
    ['delta_interval_ms', 'Translation update interval'],
    ['caption_processing_ms', 'Caption processing'],
    ['overlay_transport_ms', 'Overlay transport (app → browser)'],
    ['overlay_render_ms', 'Overlay paint (app → pixels)'],
];

function getPath(obj, path) {
    return path.split('.').reduce((o, k) => (o == null ? undefined : o[k]), obj);
}

function setPath(obj, path, value) {
    const keys = path.split('.');
    let o = obj;
    for (let i = 0; i < keys.length - 1; i++) {
        if (o[keys[i]] == null || typeof o[keys[i]] !== 'object') o[keys[i]] = {};
        o = o[keys[i]];
    }
    o[keys[keys.length - 1]] = value;
}

function esc(s) {
    return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

/** Serde enum ScreenLanguage <-> select value. */
function langToValue(l) {
    if (l && typeof l === 'object' && l.fixed) return l.fixed;
    return 'opposite';
}
function valueToLang(v) {
    return v === 'vi' || v === 'en' ? { fixed: v } : 'opposite';
}

function fmtMs(v) {
    if (v == null) return '–';
    return v >= 1000 ? (v / 1000).toFixed(2) + ' s' : Math.round(v) + ' ms';
}

export class EventConsole {
    constructor({ root, toast, isMainRunning }) {
        this.root = root;
        this.toast = toast || (() => {});
        this.isMainRunning = isMainRunning || (() => false);
        this.provider = null;            // running provider
        this.status = null;
        this.soniox = null;
        this.sonioxRetry = 0;
        this.sonioxTimer = null;
        this.pollTimer = null;
        this.saveTimer = null;
        this.pendingRestart = false;
        this.visible = false;
        this.busy = false;
        this.lastOperatorMsg = '';
        this.logCount = 0;
    }

    get live() {
        return settingsManager.get().live || {};
    }

    async init() {
        this.root.innerHTML = this._template();
        this._bind();
        this._fillForm();
        this._refreshDevices();
        this._subscribeSegments();
        this._bindHotkeys();
        this._schedulePoll();
        settingsManager.onChange(() => {
            if (!this._editing) this._fillForm();
        });
    }

    setVisible(v) {
        this.visible = v;
        this._schedulePoll(true);
    }

    // ── Template ────────────────────────────────────────────────────────
    _template() {
        const screenEditor = (id, title) => `
        <details class="ev-card" open>
          <summary>${title}</summary>
          <div class="ev-grid">
            <label>Language<select data-path="${id}.language" data-kind="lang">
              <option value="opposite">Opposite of speaker (mode A)</option>
              <option value="vi">Always Vietnamese</option>
              <option value="en">Always English</option></select></label>
            <label>Resolution<select data-res="${id}">${RES_PRESETS.map(([v, l]) => `<option value="${v}">${l}</option>`).join('')}</select></label>
            <label>Width px<input type="number" data-path="${id}.ref_width" min="16" max="16384"></label>
            <label>Height px<input type="number" data-path="${id}.ref_height" min="16" max="16384"></label>
            <label class="ev-wide">Font family<input type="text" data-path="${id}.font_family" spellcheck="false"></label>
            <label>Font size<input type="number" data-path="${id}.font_size" min="8" max="400"></label>
            <label>Weight<select data-path="${id}.font_weight" data-kind="int">
              ${[400, 500, 600, 700, 800, 900].map((w) => `<option value="${w}">${w}</option>`).join('')}</select></label>
            <label>Line height<input type="number" step="0.05" data-path="${id}.line_height" data-kind="float" min="0.8" max="3"></label>
            <label>Max lines<input type="number" data-path="${id}.max_lines" min="1" max="10"></label>
            <label>Text color<input type="color" data-path="${id}.color"></label>
            <label>Outline color<input type="color" data-path="${id}.outline_color"></label>
            <label>Outline px<input type="number" step="0.5" data-path="${id}.outline_px" data-kind="float" min="0" max="20"></label>
            <label class="ev-check"><input type="checkbox" data-path="${id}.shadow"> Drop shadow</label>
            <label class="ev-check"><input type="checkbox" data-path="${id}.backing"> Dark backing box</label>
            <label>Backing color<input type="color" data-path="${id}.backing_color"></label>
            <label>Backing opacity<input type="number" step="0.05" data-path="${id}.backing_opacity" data-kind="float" min="0" max="1"></label>
            <label>Margin X<input type="number" data-path="${id}.margin_x" min="0" max="4000"></label>
            <label>Margin Y<input type="number" data-path="${id}.margin_y" min="0" max="4000"></label>
            <label>Vertical<select data-path="${id}.valign"><option value="top">Top</option><option value="middle">Middle</option><option value="bottom">Bottom</option></select></label>
            <label>Align<select data-path="${id}.align"><option value="left">Left</option><option value="center">Center</option><option value="right">Right</option></select></label>
          </div>
        </details>`;

        const region = (r) => `
            <span class="ev-region-label">${r}</span>
            <input type="number" data-path="canvas.${r}.x" title="X">
            <input type="number" data-path="canvas.${r}.y" title="Y">
            <input type="number" data-path="canvas.${r}.width" title="Width">
            <input type="number" data-path="canvas.${r}.height" title="Height">`;

        return `
        <div class="ev-scroll">
          <section class="ev-head">
            <div class="ev-state" id="ev-state"><span class="ev-dot"></span><span id="ev-state-text">Stopped</span></div>
            <div class="ev-provider">
              <select id="ev-provider" title="Translation provider">
                <option value="openai">⚡ OpenAI Realtime (gpt-realtime-translate)</option>
                <option value="soniox">☁️ Soniox (fallback)</option>
              </select>
            </div>
            <button id="ev-start" class="ev-btn ev-primary" type="button">▶ Start live subtitles</button>
            <button id="ev-fallback" class="ev-btn" type="button" title="Emergency switch to the other provider">⇄ Switch provider</button>
          </section>
          <div id="ev-alert" class="ev-alert" hidden></div>
          <div id="ev-restart-hint" class="ev-hint" hidden>Changed settings apply on the next Start (Stop → Start).</div>

          <section class="ev-row ev-emergency">
            <button id="ev-hide" class="ev-btn" type="button" title="Ctrl+Alt+H">🙈 Hide</button>
            <button id="ev-freeze" class="ev-btn" type="button" title="Ctrl+Alt+F">❄ Freeze</button>
            <button id="ev-clear" class="ev-btn" type="button" title="Ctrl+Alt+C">🧹 Clear</button>
            <span class="ev-sep"></span>
            <span class="ev-label">Speaker language</span>
            <div class="ev-seg" id="ev-lang">
              <button type="button" data-lang="auto" title="Ctrl+Alt+A">Auto</button>
              <button type="button" data-lang="en" title="Ctrl+Alt+E">EN</button>
              <button type="button" data-lang="vi" title="Ctrl+Alt+V">VI</button>
            </div>
            <span class="ev-small" id="ev-detected"></span>
          </section>

          <section class="ev-card">
            <h4>Audio input</h4>
            <div class="ev-row">
              <select id="ev-device" data-path="input_device" class="ev-grow"></select>
              <button id="ev-devices-refresh" class="ev-btn" type="button" title="Rescan devices">⟳</button>
            </div>
            <div class="ev-meter"><div class="ev-meter-fill" id="ev-meter-fill"></div><div class="ev-meter-peak" id="ev-meter-peak"></div></div>
            <div class="ev-small" id="ev-audio-info">Not capturing</div>
            <p class="ev-note">Use the clean stage-mixer feed through the USB audio interface — not desktop/speaker audio. Nothing is played back (no TTS).</p>
          </section>

          <section class="ev-card">
            <h4>Translation</h4>
            <div class="ev-grid">
              <label>Mode<select data-path="mode">
                <option value="auto">Automatic English ↔ Vietnamese</option>
                <option value="en_to_vi">English → Vietnamese</option>
                <option value="vi_to_en">Vietnamese → English</option></select></label>
              <label>Bilingual strategy (OpenAI)<select data-path="strategy">
                <option value="dual_session">Two prewarmed sessions (fastest switch, ~2× cost)</option>
                <option value="single_switch">One session + language switch (~1× cost)</option></select></label>
              <label>Assume at start<select data-path="default_spoken"><option value="en">English speaker</option><option value="vi">Vietnamese speaker</option></select></label>
              <label>Noise reduction<select data-path="noise_reduction">
                <option value="near_field">Near field (handheld/lav mic)</option>
                <option value="far_field">Far field (room mic)</option>
                <option value="">Off (clean mixer feed)</option></select></label>
              <label>Audio chunk (ms)<input type="number" data-path="openai_chunk_ms" min="20" max="400" step="20" title="OpenAI consumes 200 ms frames and recommends 200 ms appends"></label>
            </div>
          </section>

          <section class="ev-card">
            <h4>Display mode</h4>
            <div class="ev-row">
              <button class="ev-btn" type="button" id="ev-mode-a">A · Both screens: opposite of speaker</button>
              <button class="ev-btn" type="button" id="ev-mode-b">B · Left VI / Right EN</button>
              <button class="ev-btn" type="button" id="ev-mode-b2">B · Left EN / Right VI</button>
            </div>
            <div class="ev-grid">
              <label class="ev-check"><input type="checkbox" data-path="captions.show_partials"> Show partial captions (lowest latency)</label>
              <label>Update throttle (ms)<input type="number" data-path="captions.throttle_ms" min="0" max="2000" step="10"></label>
              <label>Hold before clearing (ms, 0 = never)<input type="number" data-path="captions.hold_ms" min="0" max="600000" step="500"></label>
              <label>Finalize after silence (ms)<input type="number" data-path="captions.idle_commit_ms" min="200" max="10000" step="100"></label>
              <label>Completed sentences<select data-path="captions.final_mode">
                <option value="rollup">Roll up (keep previous lines)</option>
                <option value="replace">Replace (current sentence only)</option></select></label>
              <label class="ev-check"><input type="checkbox" data-path="captions.filter_fillers"> Hide filler words (ừm, à, oh, uh…)</label>
              <label class="ev-wide">Filler words (comma separated)<input type="text" data-path="captions.filler_words" data-kind="list" spellcheck="false"></label>
              <label>Revisable text style<select data-path="captions.tentative_style">
                <option value="dim">Dimmed</option><option value="italic">Italic</option><option value="same">Same as stable</option></select></label>
            </div>
          </section>

          ${screenEditor('left', 'Left LED subtitles')}
          ${screenEditor('right', 'Right LED subtitles')}

          <section class="ev-card">
            <h4>Overlay server (vMix Web Browser / OBS Browser Source)</h4>
            <div class="ev-small" id="ev-server-state">–</div>
            <div id="ev-urls" class="ev-urls"></div>
            <div class="ev-grid">
              <label>Port<input type="number" data-path="server.port" min="1024" max="65535"></label>
              <label class="ev-check"><input type="checkbox" data-path="server.allow_lan"> Allow other computers (LAN, token required)</label>
              <label class="ev-check"><input type="checkbox" data-path="server.auto_start"> Start with the app</label>
            </div>
            <div class="ev-row">
              <button class="ev-btn" type="button" id="ev-server-restart">Restart server</button>
              <button class="ev-btn" type="button" id="ev-copy-token">Copy control token</button>
            </div>
          </section>

          <details class="ev-card">
            <summary>Single-HDMI combined canvas (configuration A — LED processor region mapping)</summary>
            <p class="ev-note">Only works when the LED processor maps regions of one input to separate screens. A cloned HDMI cannot show different images. Coordinates below are examples — copy them from your processor's pixel map.</p>
            <div class="ev-grid">
              <label>Canvas width<input type="number" data-path="canvas.width"></label>
              <label>Canvas height<input type="number" data-path="canvas.height"></label>
              <label class="ev-check"><input type="checkbox" data-path="canvas.guides"> Show region guides (setup only)</label>
            </div>
            <div class="ev-regions">
              <span></span><span>X</span><span>Y</span><span>W</span><span>H</span>
              ${region('left')}${region('center')}${region('right')}
            </div>
          </details>

          <details class="ev-card" id="ev-perf" open>
            <summary>Performance &amp; health</summary>
            <div class="ev-small" id="ev-health"></div>
            <table class="ev-metrics"><thead><tr><th>Stage</th><th>last</th><th>median</th><th>P95</th><th>worst</th><th>n</th></tr></thead><tbody id="ev-metrics-body"></tbody></table>
            <p class="ev-note">“~” = estimated (speech onset by audio level; provider timing metadata). Mouth-to-LED latency needs external A/V measurement and is not shown.</p>
            <button class="ev-btn" type="button" id="ev-metrics-reset">Reset metrics</button>
          </details>

          <details class="ev-card">
            <summary>Caption log (finalized)</summary>
            <div id="ev-log" class="ev-log"></div>
          </details>
        </div>`;
    }

    // ── Form binding ───────────────────────────────────────────────────
    _bind() {
        const $ = (id) => this.root.querySelector('#' + id);
        this.$ = $;

        this.root.querySelectorAll('[data-path]').forEach((el) => {
            const ev = el.type === 'checkbox' || el.tagName === 'SELECT' || el.type === 'color' ? 'change' : 'input';
            el.addEventListener(ev, () => this._onField(el));
        });
        this.root.querySelectorAll('[data-res]').forEach((sel) => {
            sel.addEventListener('change', () => {
                if (sel.value === 'custom') return;
                const [w, h] = sel.value.split('x').map(Number);
                const id = sel.dataset.res;
                this._update((live) => { setPath(live, `${id}.ref_width`, w); setPath(live, `${id}.ref_height`, h); });
            });
        });

        $('ev-start').addEventListener('click', () => (this.provider ? this.stop() : this.start($('ev-provider').value)));
        $('ev-fallback').addEventListener('click', () => this.switchProvider());
        $('ev-hide').addEventListener('click', () => this.control('toggle'));
        $('ev-freeze').addEventListener('click', () => this.control('toggle-freeze'));
        $('ev-clear').addEventListener('click', () => this.control('clear'));
        this.root.querySelectorAll('#ev-lang button').forEach((b) =>
            b.addEventListener('click', () => this.control('lang-' + b.dataset.lang)));
        $('ev-devices-refresh').addEventListener('click', () => this._refreshDevices());
        $('ev-mode-a').addEventListener('click', () => this._update((l) => { l.left.language = 'opposite'; l.right.language = 'opposite'; }));
        $('ev-mode-b').addEventListener('click', () => this._update((l) => { l.left.language = { fixed: 'vi' }; l.right.language = { fixed: 'en' }; }));
        $('ev-mode-b2').addEventListener('click', () => this._update((l) => { l.left.language = { fixed: 'en' }; l.right.language = { fixed: 'vi' }; }));
        $('ev-server-restart').addEventListener('click', async () => {
            try {
                await this._flushSave();
                await invoke('live_server_restart');
                this.toast('Overlay server restarted');
            } catch (e) { this.toast(`Overlay server: ${e}`, 'error'); }
            this._poll();
        });
        $('ev-copy-token').addEventListener('click', () => this._copy(this.live.server?.access_token || '', 'Control token copied'));
        $('ev-metrics-reset').addEventListener('click', async () => { await invoke('live_reset_metrics'); this._poll(); });
        $('ev-urls').addEventListener('click', (e) => {
            const btn = e.target.closest('button[data-url]');
            if (!btn) return;
            if (btn.dataset.act === 'open') window.__TAURI__?.opener?.openUrl(btn.dataset.url);
            else this._copy(btn.dataset.url, 'URL copied — paste into vMix Web Browser input / OBS Browser Source');
        });
    }

    _onField(el) {
        const path = el.dataset.path;
        let v;
        if (el.type === 'checkbox') v = el.checked;
        else if (el.dataset.kind === 'lang') v = valueToLang(el.value);
        else if (el.dataset.kind === 'list') v = parseFillerList(el.value);
        else if (el.dataset.kind === 'float') v = parseFloat(el.value);
        else if (el.type === 'number' || el.dataset.kind === 'int') v = parseInt(el.value, 10);
        else v = el.value;
        if (typeof v === 'number' && !Number.isFinite(v)) return;
        if (this.provider && RESTART_KEYS.has(path)) this.pendingRestart = true;
        this._editing = true;
        this._update((live) => setPath(live, path, v), false);
        this._editing = false;
    }

    /** Mutate a copy of the live settings and persist (debounced). */
    _update(fn, refill = true) {
        const live = JSON.parse(JSON.stringify(this.live));
        fn(live);
        this._pendingLive = live;
        settingsManager.settings.live = live; // optimistic, so reads see it
        clearTimeout(this.saveTimer);
        this.saveTimer = setTimeout(() => this._flushSave(), 250);
        if (refill) this._fillForm();
        this.$('ev-restart-hint').hidden = !this.pendingRestart;
    }

    async _flushSave() {
        clearTimeout(this.saveTimer);
        if (!this._pendingLive) return;
        const live = this._pendingLive;
        this._pendingLive = null;
        try {
            await settingsManager.save({ live });
        } catch (e) {
            this.toast(`Could not save settings: ${e}`, 'error');
        }
    }

    _fillForm() {
        const live = this.live;
        if (!live || !this.root.firstElementChild) return;
        this.root.querySelectorAll('[data-path]').forEach((el) => {
            if (el === document.activeElement && el.type !== 'checkbox') return;
            const v = getPath(live, el.dataset.path);
            if (el.type === 'checkbox') el.checked = !!v;
            else if (el.dataset.kind === 'lang') el.value = langToValue(v);
            else if (el.dataset.kind === 'list') el.value = Array.isArray(v) ? v.join(', ') : '';
            else if (el.id === 'ev-device') this._selectDevice(v || '');
            else if (v !== undefined && v !== null) el.value = String(v);
        });
        this.root.querySelectorAll('[data-res]').forEach((sel) => {
            const id = sel.dataset.res;
            const key = `${getPath(live, id + '.ref_width')}x${getPath(live, id + '.ref_height')}`;
            sel.value = RES_PRESETS.some(([v]) => v === key) ? key : 'custom';
        });
        const p = this.$('ev-provider');
        if (!this.provider && live.provider) p.value = live.provider;
    }

    async _refreshDevices() {
        try {
            this.devices = await invoke('audio_list_input_devices');
        } catch (e) {
            this.devices = [];
        }
        const sel = this.$('ev-device');
        const cur = this.live.input_device || '';
        sel.innerHTML = `<option value="">System default input</option>` +
            this.devices.map((d) => `<option value="${esc(d.name)}">${esc(d.name)}${d.is_default ? ' (default)' : ''}${d.sample_rate ? ` · ${d.sample_rate / 1000} kHz · ${d.channels} ch` : ''}</option>`).join('');
        this._selectDevice(cur);
    }

    _selectDevice(name) {
        const sel = this.$('ev-device');
        if (name && ![...sel.options].some((o) => o.value === name)) {
            const o = document.createElement('option');
            o.value = name;
            o.textContent = `${name} (not connected)`;
            sel.appendChild(o);
        }
        sel.value = name;
    }

    // ── Start / stop / fallback ────────────────────────────────────────
    async start(provider) {
        if (this.busy) return;
        if (this.isMainRunning()) {
            this.toast('Stop the regular Live translation first (it uses the audio input).', 'error');
            return;
        }
        this.busy = true;
        this._setButtons();
        try {
            await this._flushSave();
            await settingsManager.save({ live: { ...this.live, provider } });
            await invoke('live_start', { args: { provider } });
            if (provider === 'soniox') await this._startSoniox();
            this.provider = provider;
            this.pendingRestart = false;
            this.$('ev-restart-hint').hidden = true;
            this.toast(provider === 'openai' ? 'OpenAI Realtime subtitles started' : 'Soniox subtitles started');
        } catch (e) {
            this.provider = null;
            this._stopSoniox();
            try { await invoke('live_stop'); } catch {}
            this._alert(String(e));
            this.toast(String(e), 'error');
        } finally {
            this.busy = false;
            this._setButtons();
            this._poll();
        }
    }

    async stop() {
        if (this.busy) return;
        this.busy = true;
        this._setButtons();
        try {
            this._stopSoniox();
            await invoke('live_stop');
        } catch (e) {
            this.toast(`Stop: ${e}`, 'error');
        } finally {
            this.provider = null;
            this.busy = false;
            this._setButtons();
            this._poll();
        }
    }

    /** Manual emergency fallback. Overlay URLs stay the same; screens keep the
     *  last caption until the new provider produces text. */
    async switchProvider() {
        const next = this.provider === 'openai' ? 'soniox' : 'openai';
        if (!this.provider) {
            this.$('ev-provider').value = next === 'openai' ? 'openai' : 'soniox';
            return;
        }
        if (this.busy) return;
        const from = this.provider;
        await this.stop();
        this.$('ev-provider').value = next;
        await this.start(next);
        if (this.provider === next) this.toast(`Switched ${from} → ${next}`);
    }

    async _startSoniox() {
        const s = settingsManager.get();
        const live = this.live;
        if (!s.soniox_api_key) throw new Error('Soniox API key is missing (Settings → Soniox).');
        const cfg = sonioxConfigForMode(live.mode, s.soniox_api_key);
        const client = new SonioxClient();
        this.soniox = client;
        let lastSourceLang = cfg.sourceLanguage !== 'auto' ? cfg.sourceLanguage : null;
        client.onTokens = (tokens) => {
            const { items, sourceLang } = mapSonioxTokens(tokens, { twoWay: cfg.twoWay, target: cfg.target, lastSourceLang });
            if (sourceLang) lastSourceLang = sourceLang;
            invoke('live_soniox_ingest', { items }).catch(() => {});
        };
        client.onStatusChange = (st) => {
            this.sonioxState = st;
            if (st === 'connected') this.sonioxRetry = 0;
        };
        client.onError = (msg) => {
            this.sonioxError = String(msg || '');
            this._maybeRestartSoniox();
        };
        client.connect({
            apiKey: cfg.apiKey,
            sourceLanguage: cfg.sourceLanguage,
            targetLanguage: cfg.targetLanguage,
            translationType: cfg.translationType,
            languageA: cfg.languageA,
            languageB: cfg.languageB,
            customContext: s.custom_context,
            endpointDelay: 1500,
        });
        const channel = new Channel();
        channel.onmessage = (pcm) => {
            const buf = pcm instanceof ArrayBuffer ? pcm : new Uint8Array(pcm).buffer;
            this.soniox?.sendAudio(buf);
        };
        await invoke('start_capture', { source: 'microphone', device: live.input_device || null, channel });
    }

    /** The Soniox client gives up after 3 quick retries; for long events keep
     *  trying with capped backoff unless the error is not recoverable. */
    _maybeRestartSoniox() {
        if (this.provider !== 'soniox' || !this.soniox) return;
        const fatal = /api key|credits|subscription|config error/i.test(this.sonioxError || '');
        if (fatal || this.sonioxState !== 'error' || this.sonioxTimer) return;
        const delay = Math.min(30000, 2000 * 2 ** this.sonioxRetry++);
        this.sonioxTimer = setTimeout(() => {
            this.sonioxTimer = null;
            if (this.provider !== 'soniox' || !this.soniox) return;
            const s = settingsManager.get();
            const cfg = sonioxConfigForMode(this.live.mode, s.soniox_api_key);
            this.soniox.connect({ ...cfg, customContext: s.custom_context, endpointDelay: 1500 });
        }, delay);
    }

    _stopSoniox() {
        clearTimeout(this.sonioxTimer);
        this.sonioxTimer = null;
        if (this.soniox) {
            try { this.soniox.disconnect(); } catch {}
            this.soniox = null;
            invoke('stop_capture').catch(() => {});
        }
        this.sonioxState = null;
        this.sonioxError = '';
    }

    async control(action) {
        try {
            await invoke('live_control', { action });
        } catch (e) {
            this.toast(String(e), 'error');
        }
        this._poll();
    }

    // ── Hotkeys (work from any tab while the window has focus) ─────────
    _bindHotkeys() {
        document.addEventListener('keydown', (e) => {
            if (!(e.ctrlKey && e.altKey) || e.metaKey) return;
            const map = { h: 'toggle', f: 'toggle-freeze', c: 'clear', a: 'lang-auto', e: 'lang-en', v: 'lang-vi' };
            const action = map[e.key.toLowerCase()];
            if (!action) return;
            e.preventDefault();
            this.control(action);
        });
    }

    // ── Segments log ───────────────────────────────────────────────────
    _subscribeSegments() {
        const ch = new Channel();
        ch.onmessage = (seg) => {
            const log = this.$('ev-log');
            if (!log) return;
            const row = document.createElement('div');
            row.className = 'ev-log-row';
            row.textContent = `[${seg.lang.toUpperCase()}${seg.kind === 'original' ? ' orig' : ''}] ${seg.text}`;
            log.appendChild(row);
            if (++this.logCount > 200) { log.firstChild?.remove(); this.logCount--; }
            log.scrollTop = log.scrollHeight;
        };
        invoke('live_subscribe_segments', { channel: ch }).catch(() => {});
    }

    // ── Status polling ─────────────────────────────────────────────────
    _schedulePoll(now = false) {
        clearTimeout(this.pollTimer);
        const interval = this.visible ? 250 : this.provider ? 2000 : 5000;
        this.pollTimer = setTimeout(() => this._poll(), now ? 0 : interval);
    }

    async _poll() {
        try {
            this.status = await invoke('live_status');
            this._render();
        } catch (e) {
            // backend not ready
        } finally {
            this._schedulePoll();
        }
    }

    _setButtons() {
        const b = this.$('ev-start');
        b.disabled = this.busy;
        b.textContent = this.provider ? '■ Stop live subtitles' : '▶ Start live subtitles';
        b.classList.toggle('ev-danger', !!this.provider);
        this.$('ev-provider').disabled = !!this.provider || this.busy;
        this.$('ev-fallback').textContent = this.provider === 'openai' ? '⇄ Fall back to Soniox' : this.provider === 'soniox' ? '⇄ Back to OpenAI' : '⇄ Switch provider';
        document.getElementById('event-tab-badge')?.classList.toggle('visible', !!this.provider);
    }

    _alert(msg) {
        const el = this.$('ev-alert');
        el.hidden = !msg;
        el.textContent = msg || '';
    }

    _render() {
        const st = this.status;
        if (!st) return;
        this._setButtons();
        // Provider/engine state
        let state = 'stopped', text = 'Stopped', msg = '';
        if (this.provider === 'openai' && st.engine) {
            state = st.engine.state;
            text = { live: 'LIVE · OpenAI', connecting: 'Connecting to OpenAI…', reconnecting: 'Reconnecting…', degraded: 'LIVE (one session reconnecting)', error: 'Error', stopped: 'Stopped' }[state] || state;
            msg = st.engine.message || '';
            const noSource = st.engine.lanes?.length && st.router?.mode === 'auto' && st.metrics?.counters?.translation_deltas > 20 && !st.router?.detected;
            if (noSource) msg = msg || 'Language detection has no source transcript yet — use the EN/VI buttons if the wrong language shows.';
        } else if (this.provider === 'soniox') {
            const s = this.sonioxState || 'connecting';
            state = s === 'connected' ? 'live' : s === 'error' ? 'error' : 'connecting';
            text = state === 'live' ? 'LIVE · Soniox' : state === 'error' ? 'Soniox error' : 'Connecting to Soniox…';
            msg = state === 'error' ? `Soniox: ${this.sonioxError || 'connection problem'} — retrying automatically.` : '';
        }
        const stEl = this.$('ev-state');
        stEl.dataset.state = state;
        this.$('ev-state-text').textContent = text;
        if (msg !== this.lastOperatorMsg) {
            this.lastOperatorMsg = msg;
            this._alert(msg);
        }

        // Emergency + language
        const r = st.router || {};
        this.$('ev-hide').classList.toggle('ev-on', !!r.hidden);
        this.$('ev-hide').textContent = r.hidden ? '👁 Show' : '🙈 Hide';
        this.$('ev-freeze').classList.toggle('ev-on', !!r.frozen);
        this.$('ev-freeze').textContent = r.frozen ? '▶ Unfreeze' : '❄ Freeze';
        const sel = r.override_lang || 'auto';
        this.root.querySelectorAll('#ev-lang button').forEach((b) => b.classList.toggle('active', b.dataset.lang === sel));
        const langs = (r.screen_langs || []).map((l) => (l || '–').toUpperCase());
        this.$('ev-detected').textContent = `speaking ${String(r.spoken || '?').toUpperCase()}${r.override_lang ? ' (manual)' : ''} · screens L=${langs[0] || '–'} R=${langs[1] || '–'}`;

        // Audio
        const a = st.audio;
        if (a) {
            const pct = Math.min(100, Math.round(Math.sqrt(a.rms) * 100 * 1.4));
            const peak = Math.min(100, Math.round(Math.sqrt(a.peak) * 100));
            this.$('ev-meter-fill').style.width = pct + '%';
            this.$('ev-meter-peak').style.left = peak + '%';
            this.$('ev-meter-fill').classList.toggle('clip', !!a.clipping_recent);
            const parts = [];
            if (a.state === 'running') parts.push(`${a.device || 'input'} · ${a.sample_rate ? a.sample_rate / 1000 + ' kHz' : ''}`);
            else if (a.state === 'reconnecting') parts.push(`⚠ Input lost — reconnecting${a.error ? ': ' + a.error : ''}`);
            else parts.push(a.error ? `⚠ ${a.error}` : 'Not capturing');
            if (a.clipping_recent) parts.push('⚠ CLIPPING — lower the input gain');
            if (a.state === 'running' && a.ms_since_audio != null && a.ms_since_audio > 1500) parts.push('⚠ No audio data from device');
            else if (a.state === 'running' && a.peak < 0.002) parts.push('Silent input (check mixer send)');
            if (a.device_reconnects) parts.push(`${a.device_reconnects} device reconnect(s)`);
            if (a.silence_filled_ms) parts.push(`${(a.silence_filled_ms / 1000).toFixed(1)} s silence bridged`);
            this.$('ev-audio-info').textContent = parts.join(' · ');
        }

        // Server
        const sv = st.server;
        if (sv) {
            this.$('ev-server-state').textContent = sv.running
                ? `Running on ${sv.allow_lan ? 'all interfaces (LAN, token required)' : '127.0.0.1 only'} · port ${sv.port} · ${st.metrics.overlay_clients} overlay client(s) connected`
                : `Stopped${sv.error ? ' — ' + sv.error : ''}`;
            const html = sv.urls.map(([name, url]) => `
                <div class="ev-url"><span class="ev-url-name">${esc(name)}</span><code>${esc(url)}</code>
                <button class="ev-btn" type="button" data-url="${esc(url)}" data-act="copy">Copy</button>
                <button class="ev-btn" type="button" data-url="${esc(url)}" data-act="open">Preview</button></div>`).join('');
            if (html !== this._urlsHtml) {
                this._urlsHtml = html;
                this.$('ev-urls').innerHTML = html;
            }
        }

        // Metrics
        const m = st.metrics || { series: {}, counters: {} };
        const rows = METRIC_LABELS.map(([key, label]) => {
            const s = m.series[key];
            const est = s && s.basis === 'estimated' ? '~' : '';
            return `<tr><td>${est}${label}</td><td>${s ? fmtMs(s.last_ms) : '–'}</td><td>${s ? fmtMs(s.p50_ms) : '–'}</td><td>${s ? fmtMs(s.p95_ms) : '–'}</td><td>${s ? fmtMs(s.max_ms) : '–'}</td><td>${s ? s.count : 0}</td></tr>`;
        }).join('');
        const first = m.series.time_to_first_delta_ms, paint = m.series.overlay_render_ms;
        const e2e = first ? first.p50_ms + (paint ? paint.p50_ms : 0) : null;
        const rowsAll = rows + `<tr class="ev-sum"><td>~Speech onset → caption painted (median sum)</td><td></td><td>${e2e != null ? fmtMs(e2e) : '–'}</td><td></td><td></td><td></td></tr>`;
        if (this.visible && rowsAll !== this._metricsHtml) {
            this._metricsHtml = rowsAll;
            this.$('ev-metrics-body').innerHTML = rowsAll;
        }
        const c = m.counters || {};
        const lanes = (st.engine?.lanes || []).map((l) => `→${l.target}: ${l.state}${l.reconnects ? ` (${l.reconnects} reconnects)` : ''}${l.expires_in_s != null ? `, renews in ${Math.max(0, l.expires_in_s - 90)} s` : ''}`).join(' · ');
        this.$('ev-health').textContent = [
            `Provider: ${this.provider || 'none'}`,
            lanes,
            `uptime ${Math.floor(st.uptime_s / 60)} min`,
            `reconnects ${c.reconnects || 0}`,
            `rotations ${c.session_rotations || 0}`,
            `dropped audio chunks ${c.audio_chunks_dropped || 0}`,
            st.memory_mb != null ? `memory ${st.memory_mb.toFixed(0)} MB` : '',
        ].filter(Boolean).join(' · ');
    }

    async _copy(text, msg) {
        try {
            await navigator.clipboard.writeText(text);
            this.toast(msg);
        } catch {
            this.toast(text);
        }
    }

    /** App close: stop the webview-side provider; Rust shuts down the rest. */
    shutdown() {
        this._stopSoniox();
    }
}
