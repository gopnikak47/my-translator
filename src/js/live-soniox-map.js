/**
 * Map one Soniox response's tokens to caption-hub ingest items.
 *
 * Soniox semantics: each response carries the NEW final tokens plus the FULL
 * current set of non-final tokens (which replace the previous non-final set).
 * So `stable` is appended, `tentative` replaces — exactly what the Rust caption
 * hub expects. Tentative is always sent (an empty string clears it).
 *
 * @param {Array} tokens  Soniox `tokens` array
 * @param {{twoWay:boolean, target:string, lastSourceLang?:string}} cfg
 * @returns {{items:Array, sourceLang:(string|null)}}
 */
export function mapSonioxTokens(tokens, cfg) {
    let sStable = '', sTent = '', tStable = '', tTent = '';
    let sLang = null, tLang = null, end = false;
    for (const tok of tokens || []) {
        if (!tok || typeof tok.text !== 'string') continue;
        if (tok.text === '<end>' || tok.text === '<fin>') { end = true; continue; }
        if (tok.translation_status === 'translation') {
            tLang = tok.language || tLang;
            if (tok.is_final) tStable += tok.text; else tTent += tok.text;
        } else {
            // 'original', 'none' (third language in two-way) or no translation info.
            if (tok.language) sLang = tok.language;
            if (tok.is_final) sStable += tok.text; else sTent += tok.text;
        }
    }
    const srcLang = sLang || cfg.lastSourceLang || null;
    if (!tLang) {
        if (cfg.twoWay) {
            tLang = srcLang === 'vi' ? 'en' : srcLang === 'en' ? 'vi' : null;
        } else {
            tLang = cfg.target || null;
        }
    }
    const items = [
        { stream: 'source', lang: srcLang, stable: sStable, tentative: sTent, end },
        { stream: 'translation', lang: tLang, stable: tStable, tentative: tTent, end },
    ];
    return { items, sourceLang: srcLang };
}

/** Soniox client config for a live-event translation mode. */
export function sonioxConfigForMode(mode, apiKey) {
    if (mode === 'en_to_vi') {
        return { apiKey, sourceLanguage: 'en', targetLanguage: 'vi', translationType: 'one_way', twoWay: false, target: 'vi' };
    }
    if (mode === 'vi_to_en') {
        return { apiKey, sourceLanguage: 'vi', targetLanguage: 'en', translationType: 'one_way', twoWay: false, target: 'en' };
    }
    return { apiKey, sourceLanguage: 'auto', targetLanguage: 'vi', translationType: 'two_way', languageA: 'en', languageB: 'vi', twoWay: true, target: null };
}
