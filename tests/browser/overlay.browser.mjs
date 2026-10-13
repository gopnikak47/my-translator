/**
 * Browser test of the broadcast overlay in real Chromium (the engine family
 * embedded by vMix Web Browser inputs and OBS Browser Sources).
 *
 * Needs Playwright (not a project dependency):
 *   (cd src-tauri && cargo build -p live-core --example overlay_demo)
 *   NODE_PATH=$(npm root -g) node tests/browser/overlay.browser.mjs [outdir]
 *
 * Checks: transparent page, captions stream without reloads, both/canvas
 * layouts, emergency hide, client auto-reconnect after an app restart, and
 * collects the overlay's own transport/paint latency samples.
 */
import { spawn } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

// CommonJS resolution honours NODE_PATH (global Playwright install).
const { chromium } = createRequire(import.meta.url)('playwright');

const PORT = 18765;
const BASE = `http://127.0.0.1:${PORT}`;
const TOKEN = 'demo-token-0123456789';
const outDir = process.argv[2] || 'overlay-test-output';
const demoBin = path.resolve('src-tauri/target/debug/examples/overlay_demo');
mkdirSync(outDir, { recursive: true });

let failures = 0;
const check = (ok, msg) => {
    console.log(`${ok ? 'PASS' : 'FAIL'}  ${msg}`);
    if (!ok) failures++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function startDemo(mode = 'mode-a') {
    const p = spawn(demoBin, [String(PORT), mode], { stdio: ['ignore', 'pipe', 'inherit'] });
    p.stdout.on('data', (d) => process.stdout.write(`[demo] ${d}`));
    return p;
}

async function waitHealthy() {
    for (let i = 0; i < 100; i++) {
        try { if ((await fetch(`${BASE}/health`)).ok) return true; } catch {}
        await sleep(100);
    }
    return false;
}

const launchOpts = {};

let demo = startDemo();
check(await waitHealthy(), 'overlay server is up');

const browser = await chromium.launch(launchOpts);
try {
    const left = await browser.newPage({ viewport: { width: 960, height: 1080 } });
    await left.goto(`${BASE}/overlay?screen=left`);
    const bg = await left.evaluate(() => getComputedStyle(document.body).backgroundColor);
    check(bg === 'rgba(0, 0, 0, 0)' || bg === 'transparent', `page background is transparent (${bg})`);

    // Text appears and grows word by word without page reloads.
    let navs = 0;
    left.on('framenavigated', () => navs++);
    await left.waitForFunction(() => document.querySelector('.s')?.textContent.length > 5, null, { timeout: 8000 });
    const t1 = await left.textContent('.inner');
    await sleep(700);
    const t2 = await left.textContent('.inner');
    check(t1 !== t2, `captions update live ("${t1.slice(-30)}" → "${t2.slice(-30)}")`);
    check(navs === 0, 'no page reloads while streaming');

    // Max-lines: once the caption is long, exactly 2 lines are shown (newest words).
    await left.waitForFunction(() => document.querySelector('.s').textContent.length > 40, null, { timeout: 15000 });
    await sleep(1500);
    const lines = await left.evaluate(() => {
        const t = document.querySelector('.text');
        const lh = parseFloat(getComputedStyle(document.querySelector('.box')).lineHeight);
        return Math.round(t.getBoundingClientRect().height / lh);
    });
    check(lines === 2, `long caption shows exactly max_lines = 2 lines (rendered ${lines})`);

    const right = await browser.newPage({ viewport: { width: 960, height: 1080 } });
    await right.goto(`${BASE}/overlay?screen=right`);
    const both = await browser.newPage({ viewport: { width: 1920, height: 1080 } });
    await both.goto(`${BASE}/overlay?screen=both`);
    const canvas = await browser.newPage({ viewport: { width: 1920, height: 540 } });
    await canvas.goto(`${BASE}/overlay?screen=canvas&guides=1`);
    await sleep(2500);

    const shot = async (page, name) => {
        // Screenshot on a grey "stage" so white text is visible; the live page stays transparent.
        await page.addStyleTag({ content: 'html{background:#3a4150 !important}' });
        await page.screenshot({ path: path.join(outDir, name) });
        await page.addStyleTag({ content: 'html{background:transparent !important}' });
    };
    await shot(left, 'overlay-left.png');
    await shot(both, 'overlay-both.png');
    await shot(canvas, 'overlay-canvas-guides.png');
    const canvasRegions = await canvas.evaluate(() => [...document.querySelectorAll('.region')].map((r) => r.getBoundingClientRect().x));
    check(canvasRegions.length === 2 && canvasRegions[1] > canvasRegions[0], `canvas places left/right regions (${canvasRegions.map(Math.round)})`);

    // Emergency hide through the control API.
    const hide = await fetch(`${BASE}/control/hide`, { method: 'POST', headers: { 'X-Access-Token': TOKEN } });
    check(hide.ok, 'control API accepts token');
    await left.waitForFunction(() => document.querySelector('.region').style.visibility === 'hidden', null, { timeout: 3000 })
        .then(() => check(true, 'emergency hide reaches the overlay'), () => check(false, 'emergency hide reaches the overlay'));
    await fetch(`${BASE}/control/show`, { method: 'POST', headers: { 'X-Access-Token': TOKEN } });
    const unauth = await fetch(`${BASE}/control/hide`, { method: 'POST' });
    check(unauth.status === 401, 'control API rejects requests without token');

    // Let the overlays report latency (every 5 s), then restart the "app".
    await sleep(6000);
    demo.kill('SIGTERM');
    await sleep(1500);
    demo = startDemo();
    check(await waitHealthy(), 'app restarted');
    const before = await left.textContent('.inner');
    await left.waitForFunction((b) => document.querySelector('.inner').textContent !== b && document.querySelector('.s').textContent.length > 0,
        before, { timeout: 10000 })
        .then(() => check(true, 'overlay reconnected automatically after app restart (no reload)'),
              () => check(false, 'overlay reconnected automatically after app restart'));
    check(navs === 0, 'still no page reloads after reconnect');
    await sleep(12000); // two report periods → demo prints paint latency
} finally {
    await browser.close();
    demo.kill('SIGTERM');
}
console.log(failures ? `\n${failures} check(s) FAILED` : '\nAll browser checks passed');
process.exit(failures ? 1 : 0);
