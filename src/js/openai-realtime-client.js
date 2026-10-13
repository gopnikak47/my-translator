// OpenAI Realtime Translate WebSocket client (via Tauri Rust backend).
// Public callback API mirrors src/js/soniox.js so app.js can branch on provider.

const { invoke, Channel } = window.__TAURI__.core;

/** Index just past the last sentence terminator that is followed by whitespace (0 = none). */
export function lastSentenceEnd(text) {
    const re = /[.!?…。]["'”’)]*\s/g;
    let m, end = 0;
    while ((m = re.exec(text)) !== null) end = m.index + m[0].length;
    return end;
}

export class OpenAiRealtimeClient {
    constructor() {
        this.sessionId = null;
        this.channel = null;
        this.outputQueue = null;
        this.isConnected = false;

        this.onStatusChange = () => {};
        // (sourceText, translatedText) — emitted when output sentence finalizes.
        // sourceText is the source-language transcript that was buffered alongside.
        this.onSegment = () => {};
        this.onProvisional = () => {};         // live target deltas
        this.onSourceProvisional = () => {};   // live source deltas
        this.onError = () => {};
        this.onClosed = () => {};

        this._provisionalBuffer = '';
        this._sourceBuffer = '';
        // Finalized source-language transcripts (from session.input_transcript.done)
        // waiting to be paired with the next translated target final. Whisper and
        // translation finalize on independent cadences, so a queue keeps pairs aligned.
        this._pendingSourceFinals = [];
        this._muted = false;
    }

    setMuted(muted) {
        this._muted = !!muted;
        if (this._muted) this.outputQueue?.flush();
    }

    async connect(cfg, outputQueue) {
        this.outputQueue = outputQueue;
        this._muted = cfg.audioOutput === false;
        this.channel = new Channel();
        this.channel.onmessage = (evt) => this._handleEvent(evt);

        try {
            this.sessionId = await invoke('openai_realtime_start', {
                config: {
                    api_key: cfg.apiKey,
                    source_language: cfg.sourceLanguage || 'auto',
                    target_language: cfg.targetLanguage,
                    voice: cfg.voice || null,
                    audio_output: cfg.audioOutput !== false,
                },
                onEvent: this.channel,
            });
            this.isConnected = true;
        } catch (err) {
            this.onError('connect_failed', String(err));
            throw err;
        }
    }

    async sendAudio(arrayBuffer) {
        if (!this.isConnected || this.sessionId == null) return;
        try {
            // Raw binary IPC body (no JSON number array).
            await invoke('openai_realtime_send_audio', new Uint8Array(arrayBuffer), {
                headers: { 'x-session-id': String(this.sessionId) },
            });
        } catch (err) {
            console.warn('[OpenAI Realtime] send audio failed:', err);
        }
    }

    /**
     * If there's an in-flight partial translation that never got finalized
     * (user pressed Stop mid-sentence), emit it as a final segment so it's
     * persisted instead of lost. Safe to call multiple times.
     */
    flushPending() {
        // Emit any queued source finals as source-only segments (no translation
        // arrived for them yet). Then emit a final pair using whatever in-flight
        // buffers remain — keeps stop-mid-sentence text from being silently dropped.
        try {
            while (this._pendingSourceFinals.length > 1) {
                this.onSegment(this._pendingSourceFinals.shift(), '');
            }
            const tgt = this._provisionalBuffer;
            const src = this._pendingSourceFinals.shift() || this._sourceBuffer;
            this._provisionalBuffer = '';
            this._sourceBuffer = '';
            if (tgt || src) this.onSegment(src, tgt);
        } catch (e) {
            console.error('[OpenAI flush]', e);
        }
    }

    async disconnect() {
        if (!this.isConnected) return;
        this.isConnected = false;
        this.flushPending();
        try {
            await invoke('openai_realtime_stop', { sessionId: this.sessionId });
        } catch {}
        this.outputQueue?.flush();
    }

    _handleEvent(evt) {
        switch (evt.type) {
            case 'status':
                this.onStatusChange(evt.state, evt.message);
                break;
            case 'transcript':
                if (evt.is_final) {
                    // Prefer the next queued source final (authoritative, full text
                    // from whisper). Fall back to accumulated deltas if the source
                    // .done event hasn't arrived yet.
                    const sourceText = this._pendingSourceFinals.shift() || this._sourceBuffer;
                    this._provisionalBuffer = '';
                    this._sourceBuffer = '';
                    this.onSegment(sourceText, evt.text);
                } else {
                    this._provisionalBuffer += evt.text;
                    // The translation API streams append-only deltas and has no
                    // "done" event, so finalize completed sentences here
                    // (terminator followed by whitespace) to keep segments short.
                    const cut = lastSentenceEnd(this._provisionalBuffer);
                    if (cut > 0) {
                        const done = this._provisionalBuffer.slice(0, cut).trim();
                        this._provisionalBuffer = this._provisionalBuffer.slice(cut).replace(/^\s+/, '');
                        const sourceText = this._pendingSourceFinals.shift() || this._sourceBuffer;
                        this._sourceBuffer = '';
                        if (done) this.onSegment(sourceText, done);
                    }
                    this.onProvisional(this._provisionalBuffer);
                }
                break;
            case 'source_transcript':
                if (evt.is_final) {
                    // Whisper finalized the input chunk — queue the full transcript
                    // for pairing with the next translated final. Drop the delta
                    // buffer since we now have the authoritative text.
                    this._pendingSourceFinals.push(evt.text);
                    this._sourceBuffer = '';
                    this.onSourceProvisional(evt.text);
                } else {
                    this._sourceBuffer += evt.text;
                    this.onSourceProvisional(this._sourceBuffer);
                }
                break;
            case 'audio_chunk':
                if (!this._muted) this.outputQueue?.push(evt.pcm_base64);
                break;
            case 'error':
                this.onError(evt.code, evt.message);
                break;
            case 'closed':
                this.isConnected = false;
                this.onClosed(evt.reason);
                break;
        }
    }
}
