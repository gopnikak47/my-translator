import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mapSonioxTokens, sonioxConfigForMode } from '../../src/js/live-soniox-map.js';

const tok = (text, opts = {}) => ({ text, is_final: false, ...opts });

test('two-way: source and translation split, final appended, non-final replaces', () => {
    const { items, sourceLang } = mapSonioxTokens([
        tok('Hello', { is_final: true, translation_status: 'original', language: 'en' }),
        tok(' every', { translation_status: 'original', language: 'en' }),
        tok('Xin chào', { is_final: true, translation_status: 'translation', language: 'vi' }),
    ], { twoWay: true, target: null });
    assert.equal(sourceLang, 'en');
    assert.deepEqual(items[0], { stream: 'source', lang: 'en', stable: 'Hello', tentative: ' every', end: false });
    assert.deepEqual(items[1], { stream: 'translation', lang: 'vi', stable: 'Xin chào', tentative: '', end: false });
});

test('end token marks segment end and is not shown as text', () => {
    const { items } = mapSonioxTokens([
        tok(' world.', { is_final: true, translation_status: 'original', language: 'en' }),
        tok('<end>', { is_final: true }),
    ], { twoWay: false, target: 'vi' });
    assert.equal(items[0].stable, ' world.');
    assert.equal(items[0].end, true);
    assert.equal(items[1].lang, 'vi');
    assert.ok(!items[0].stable.includes('<end>'));
});

test('translation language falls back to opposite of last source language', () => {
    const { items } = mapSonioxTokens([
        tok('Thank you', { is_final: true, translation_status: 'translation' }),
    ], { twoWay: true, target: null, lastSourceLang: 'vi' });
    assert.equal(items[1].lang, 'en');
});

test('empty non-final set clears tentative text', () => {
    const { items } = mapSonioxTokens([tok('ok', { is_final: true, translation_status: 'original', language: 'en' })], { twoWay: false, target: 'vi' });
    assert.equal(items[0].tentative, '');
    assert.equal(items[1].tentative, '');
});

test('ignores malformed tokens', () => {
    const { items } = mapSonioxTokens([null, { foo: 1 }, tok('a', { is_final: true })], { twoWay: false, target: 'vi' });
    assert.equal(items[0].stable, 'a');
});

test('mode → Soniox config', () => {
    assert.equal(sonioxConfigForMode('en_to_vi', 'k').targetLanguage, 'vi');
    assert.equal(sonioxConfigForMode('vi_to_en', 'k').sourceLanguage, 'vi');
    const auto = sonioxConfigForMode('auto', 'k');
    assert.equal(auto.translationType, 'two_way');
    assert.deepEqual([auto.languageA, auto.languageB], ['en', 'vi']);
});
