import { test } from 'node:test';
import assert from 'node:assert/strict';
import { DEFAULT_FILLERS, makeFillerFilter, parseFillerList } from '../../src/js/filler-filter.js';

const f = makeFillerFilter(DEFAULT_FILLERS);

// Same cases as live-core/src/filler.rs — both implementations must agree.
test('removes Vietnamese and English fillers', () => {
    assert.equal(f('Ừm, chúng ta bắt đầu nhé'), 'Chúng ta bắt đầu nhé');
    assert.equal(f('Thank you, uh, everyone'), 'Thank you, everyone');
    assert.equal(f('Oh, welcome to the show.'), 'Welcome to the show.');
    assert.equal(f('Hôm nay à, chúng ta'), 'Hôm nay chúng ta');
});

test('stretched forms and repeats', () => {
    assert.equal(f('Ừmmm ừm, ờ, xin chào'), 'Xin chào');
    assert.equal(f('so ummm we start'), 'so we start');
    assert.equal(f('hmmm okay'), 'okay');
});

test('moves sentence end to previous word', () => {
    assert.equal(f('Vậy à? Tốt quá'), 'Vậy? Tốt quá');
    assert.equal(f('That is it, uh.'), 'That is it.');
});

test('keeps words that only contain fillers or carry meaning', () => {
    assert.equal(f('Ohio and Erik are here'), 'Ohio and Erik are here');
    assert.equal(f('ừ, được rồi ạ'), 'ừ, được rồi ạ');
    assert.equal(f('cái màn hình'), 'cái màn hình');
});

test('whitespace and empty results', () => {
    assert.equal(f(' uh hello '), ' hello ');
    assert.equal(f('ừm...'), '');
    assert.equal(f('   '), '   ');
    assert.equal(makeFillerFilter([])('um ok'), 'um ok');
});

test('decomposed Unicode input still matches (NFD)', () => {
    assert.equal(f('ừm chào'.normalize('NFD') ), 'chào'.normalize('NFD'));
});

test('parse list from settings text', () => {
    assert.deepEqual(parseFillerList('ừm, à ,oh;\nuh, kiểu như'), ['ừm', 'à', 'oh', 'uh']);
});
