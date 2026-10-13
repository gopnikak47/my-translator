import { test, before } from 'node:test';
import assert from 'node:assert/strict';

let mod;
before(async () => {
    // Minimal Tauri global so the module can load outside the webview.
    globalThis.window = { __TAURI__: { core: { invoke: async () => 1, Channel: class { } } } };
    mod = await import('../../src/js/openai-realtime-client.js');
});

test('lastSentenceEnd finds terminators followed by whitespace only', () => {
    const { lastSentenceEnd } = mod;
    assert.equal(lastSentenceEnd('Giá là 3.5 triệu'), 0);
    assert.equal(lastSentenceEnd('Hello. World'), 7);
    assert.equal(lastSentenceEnd('One. Two! Three'), 10);
    assert.equal(lastSentenceEnd('He said "hi." Then'), 14);
    assert.equal(lastSentenceEnd('Trailing.'), 0);
});

test('deltas are finalized per sentence without duplication', () => {
    const c = new mod.OpenAiRealtimeClient();
    const segments = [];
    const provisional = [];
    c.onSegment = (src, tgt) => segments.push([src, tgt]);
    c.onProvisional = (t) => provisional.push(t);
    c._handleEvent({ type: 'source_transcript', text: 'Hello everyone.', is_final: false });
    for (const d of ['Xin chào', ' mọi người.', ' Hôm nay', ' chúng ta']) {
        c._handleEvent({ type: 'transcript', text: d, is_final: false });
    }
    assert.deepEqual(segments, [['Hello everyone.', 'Xin chào mọi người.']]);
    assert.equal(provisional.at(-1), 'Hôm nay chúng ta');
    c.flushPending();
    assert.deepEqual(segments.at(-1), ['', 'Hôm nay chúng ta']);
});
