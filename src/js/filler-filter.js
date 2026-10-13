/**
 * Filler-word filter ("ừm", "à", "oh", "uh"…) for displayed captions.
 * Mirrors src-tauri/live-core/src/filler.rs so the main window and the
 * vMix/OBS overlays behave identically.
 *
 * - Only whole words are removed ("Ohio" stays); stretched forms match
 *   ("ừmmm" → "ừm", "uhhh" → "uh").
 * - A sentence-final mark on a removed word moves to the previous word
 *   ("Vậy à?" → "Vậy?"); a filler that started a sentence capitalises the
 *   next word ("Ừm, chúng ta" → "Chúng ta").
 */

// Excludes meaningful Vietnamese words: "ừ" (yes), "ạ" (politeness).
export const DEFAULT_FILLERS = ['ừm', 'ờm', 'ưm', 'hừm', 'ờ', 'à', 'um', 'uhm', 'uh', 'er', 'erm', 'ah', 'oh', 'hmm'];

const EDGE = /^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu;

export function normalizeToken(w) {
    const core = String(w).normalize('NFC').replace(EDGE, '').toLowerCase();
    let out = '';
    let prev = '';
    for (const c of core) {
        if (c !== prev) out += c;
        prev = c;
    }
    return out;
}

/** Build a filter function; returns identity when the list is empty. */
export function makeFillerFilter(words) {
    const set = new Set((words || []).map(normalizeToken).filter(Boolean));
    if (!set.size) return (t) => t;
    return (text) => applyFillerFilter(text, set);
}

export function applyFillerFilter(text, set) {
    if (!text || !text.trim()) return text;
    const lead = text.match(/^\s*/)[0];
    const trail = text.match(/\s*$/)[0];
    const out = [];
    let capitalizeNext = false;
    let removed = false;
    for (const w of text.trim().split(/\s+/)) {
        const n = normalizeToken(w);
        if (n && set.has(n)) {
            removed = true;
            const trailing = (w.match(/[^\p{L}\p{N}]+$/u) || [''])[0];
            const terminal = trailing.replace(/[^.?!…]/g, '');
            if (terminal && out.length) {
                let prev = out[out.length - 1];
                if (/[,;:]$/.test(prev)) prev = prev.slice(0, -1);
                if (/[\p{L}\p{N}]$/u.test(prev)) prev += terminal;
                out[out.length - 1] = prev;
            }
            const firstLetter = (w.match(/\p{L}/u) || [''])[0];
            if (firstLetter && firstLetter !== firstLetter.toLowerCase()) capitalizeNext = true;
            continue;
        }
        if (capitalizeNext) {
            capitalizeNext = false;
            out.push(w.replace(/\p{L}/u, (c) => c.toUpperCase()));
        } else {
            out.push(w);
        }
    }
    if (!removed) return text;
    if (!out.length) return '';
    return lead + out.join(' ') + trail;
}

/** "ừm, à, oh" → ['ừm', 'à', 'oh'] */
export function parseFillerList(str) {
    return String(str || '')
        .split(/[,\n;]+/)
        .map((s) => s.trim())
        .filter((s) => s && s.length <= 32 && !/\s/.test(s));
}
