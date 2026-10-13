/*
 * My Translator — broadcast subtitle overlay (vMix Web Browser input / OBS Browser Source).
 *
 * Receives caption frames over Server-Sent Events from the local app and renders
 * subtitles on a transparent page. No polling, no animation, no external assets.
 * Text nodes are only touched when their content actually changes, and all DOM
 * writes for one update happen inside a single requestAnimationFrame.
 *
 * URL parameters:
 *   screen = left | right | both | canvas   (default: both)
 *   token  = access token (only needed when the app runs in LAN mode)
 *   guides = 1  → draw region outlines on the canvas page (alignment only)
 */
(function () {
  'use strict';

  var params = new URLSearchParams(location.search);
  var screen = (params.get('screen') || 'both').toLowerCase();
  if (['left', 'right', 'both', 'canvas'].indexOf(screen) < 0) screen = 'both';
  var token = params.get('token') || '';
  var guides = params.get('guides') === '1';
  var stage = document.getElementById('stage');

  var layout = null;
  var frames = { left: null, right: null };
  var regions = {};          // id -> { region, box, s, t, last:{stable,tentative,visible} }
  var dirty = false;
  var pendingMeasure = [];   // t_pub_ms of frames awaiting paint
  var stats = { transport: [], render: [] };
  var holdTimer = null;
  var disconnectTimer = null;

  function qs(extra) {
    var p = new URLSearchParams(extra || {});
    if (token) p.set('token', token);
    var s = p.toString();
    return s ? '?' + s : '';
  }

  // ── Layout ────────────────────────────────────────────────────────────
  function outlineShadow(px, color, drop) {
    var parts = [];
    if (px > 0) {
      var steps = px > 2 ? 16 : 8;
      for (var i = 0; i < steps; i++) {
        var a = (i / steps) * Math.PI * 2;
        parts.push((Math.cos(a) * px).toFixed(2) + 'px ' + (Math.sin(a) * px).toFixed(2) + 'px 0 ' + color);
      }
    }
    if (drop) parts.push('0 ' + Math.max(2, px).toFixed(1) + 'px ' + Math.max(4, px * 2).toFixed(1) + 'px rgba(0,0,0,0.8)');
    return parts.length ? parts.join(',') : 'none';
  }

  function hexToRgba(hex, alpha) {
    var h = String(hex || '#000').replace('#', '');
    if (h.length === 3) h = h[0] + h[0] + h[1] + h[1] + h[2] + h[2];
    var n = parseInt(h.slice(0, 6), 16);
    if (isNaN(n)) n = 0;
    return 'rgba(' + ((n >> 16) & 255) + ',' + ((n >> 8) & 255) + ',' + (n & 255) + ',' + alpha + ')';
  }

  function makeRegion(id) {
    var region = document.createElement('div');
    region.className = 'region';
    var box = document.createElement('div');
    box.className = 'box empty';
    var text = document.createElement('div');
    text.className = 'text';
    var inner = document.createElement('div');
    inner.className = 'inner';
    var s = document.createElement('span');
    s.className = 's';
    var t = document.createElement('span');
    t.className = 't';
    s.appendChild(document.createTextNode(''));
    t.appendChild(document.createTextNode(''));
    inner.appendChild(s);
    inner.appendChild(t);
    text.appendChild(inner);
    box.appendChild(text);
    region.appendChild(box);
    stage.appendChild(region);
    regions[id] = { region: region, box: box, text: text, s: s.firstChild, t: t, tn: t.firstChild, last: { stable: null, tentative: null, visible: null } };
    return regions[id];
  }

  // Apply one ScreenStyle to a region rectangle (all values in page px).
  function styleRegion(r, st, rect, scale) {
    var R = r.region.style;
    R.left = rect.x + 'px';
    R.top = rect.y + 'px';
    R.width = rect.w + 'px';
    R.height = rect.h + 'px';
    R.padding = (st.margin_y * scale) + 'px ' + (st.margin_x * scale) + 'px';
    R.alignItems = st.valign === 'top' ? 'flex-start' : st.valign === 'middle' ? 'center' : 'flex-end';
    R.justifyContent = st.align === 'left' ? 'flex-start' : st.align === 'right' ? 'flex-end' : 'center';

    var fs = st.font_size * scale;
    var lh = Math.round(fs * st.line_height);
    var B = r.box.style;
    B.fontFamily = st.font_family;
    B.fontSize = fs + 'px';
    B.fontWeight = String(st.font_weight);
    B.lineHeight = lh + 'px';
    B.color = st.color;
    B.textAlign = st.align;
    B.textShadow = outlineShadow(st.outline_px * scale, st.outline_color, st.shadow);
    if (st.backing) {
      B.background = hexToRgba(st.backing_color, st.backing_opacity);
      B.padding = (0.15 * fs) + 'px ' + (0.45 * fs) + 'px';
      B.borderRadius = (0.12 * fs) + 'px';
    } else {
      B.background = 'transparent';
      B.padding = '0';
    }
    // Exactly max_lines lines visible; older lines overflow upwards and are clipped.
    r.maxH = lh * st.max_lines;
    r.text.style.maxHeight = (r.maxH + 2) + 'px';
    r.tn.parentNode.className = 't ' + (layout.tentative_style === 'dim' ? 'dim' : layout.tentative_style === 'italic' ? 'italic' : '');
  }

  function applyLayout() {
    if (!layout) return;
    var W = window.innerWidth, H = window.innerHeight;
    var ids = screen === 'both' ? ['left', 'right'] : screen === 'canvas' ? ['left', 'right'] : [screen];
    ids.forEach(function (id) { if (!regions[id]) makeRegion(id); });
    Array.prototype.slice.call(stage.querySelectorAll('.guide')).forEach(function (g) { g.remove(); });
    stage.style.width = W + 'px';
    stage.style.height = H + 'px';

    if (screen === 'canvas') {
      var c = layout.canvas;
      var k = Math.min(W / c.width, H / c.height);
      ['left', 'right'].forEach(function (id) {
        var rg = c[id], st = layout[id];
        var rect = { x: rg.x * k, y: rg.y * k, w: rg.width * k, h: rg.height * k };
        var s = k * Math.min(rg.width / st.ref_width, rg.height / st.ref_height);
        styleRegion(regions[id], st, rect, s);
      });
      if (guides || c.guides) {
        [['left', c.left], ['center', c.center], ['right', c.right]].forEach(function (p) {
          var g = document.createElement('div');
          g.className = 'guide';
          g.style.left = p[1].x * k + 'px'; g.style.top = p[1].y * k + 'px';
          g.style.width = p[1].width * k + 'px'; g.style.height = p[1].height * k + 'px';
          g.textContent = p[0] + ' ' + p[1].x + ',' + p[1].y + ' ' + p[1].width + '×' + p[1].height;
          stage.appendChild(g);
        });
      }
    } else if (screen === 'both') {
      var st = layout.left;
      var s0 = Math.min(W / st.ref_width, H / st.ref_height);
      if (sameCaption()) {
        styleRegion(regions.left, st, { x: 0, y: 0, w: W, h: H }, s0);
        regions.right.region.style.display = 'none';
      } else {
        regions.right.region.style.display = '';
        styleRegion(regions.left, st, { x: 0, y: 0, w: W, h: H / 2 }, s0);
        var st2 = layout.right;
        styleRegion(regions.right, st2, { x: 0, y: H / 2, w: W, h: H / 2 }, Math.min(W / st2.ref_width, H / st2.ref_height));
      }
    } else {
      var st3 = layout[screen];
      styleRegion(regions[screen], st3, { x: 0, y: 0, w: W, h: H }, Math.min(W / st3.ref_width, H / st3.ref_height));
    }
    Object.keys(regions).forEach(function (id) { regions[id].last = { stable: null, tentative: null, visible: null }; });
    schedule();
  }

  // In "both" mode, screens configured to show the same language (display
  // mode A) are shown once; different languages (mode B) get two blocks.
  // Decided from configuration, not content, so the layout never flips while
  // the two screens' frames arrive a few milliseconds apart.
  function sameCaption() {
    if (!layout) return true;
    return JSON.stringify(layout.left.language) === JSON.stringify(layout.right.language);
  }

  // ── Rendering ─────────────────────────────────────────────────────────
  function schedule() {
    if (dirty) return;
    dirty = true;
    requestAnimationFrame(render);
  }

  function render() {
    dirty = false;
    if (!layout) return;
    var anyText = false;
    Object.keys(regions).forEach(function (id) {
      var r = regions[id], f = frames[id];
      if (!f) return;
      var L = r.last;
      if (L.stable !== f.stable || L.tentative !== f.tentative) {
        r.s.nodeValue = f.stable;
        r.tn.nodeValue = f.tentative;
        fitLines(r);
        L.stable = f.stable;
        L.tentative = f.tentative;
      }
      var empty = !f.stable && !f.tentative;
      r.box.classList.toggle('empty', empty);
      if (L.visible !== f.visible) { r.region.style.visibility = f.visible ? 'visible' : 'hidden'; L.visible = f.visible; }
      if (!empty) anyText = true;
    });
    armHold(anyText);
    if (pendingMeasure.length) {
      var list = pendingMeasure;
      pendingMeasure = [];
      // Next frame ≈ after this frame was composited.
      requestAnimationFrame(function () {
        var now = Date.now();
        list.forEach(function (t) { push(stats.render, now - t); });
      });
    }
  }

  // Show exactly the newest max_lines lines: drop leading words until the text
  // fits (binary search, a few cheap reflows of two lines of text). Clipping
  // overflow instead would let descenders of the hidden line peek in.
  function fitLines(r) {
    var limit = r.maxH;
    if (!limit) return;
    var inner = r.text.firstChild;
    if (inner.offsetHeight <= limit + 1) return;
    var parts = [r.s, r.tn];
    for (var p = 0; p < parts.length; p++) {
      var node = parts[p];
      var words = (node.nodeValue || '').split(/(?=\s)/);
      var lo = 0, hi = words.length;
      node.nodeValue = '';
      if (inner.offsetHeight > limit + 1) continue; // even without this part it overflows
      while (lo < hi) {
        var mid = (lo + hi) >> 1;
        node.nodeValue = words.slice(mid).join('').replace(/^\s+/, '');
        if (inner.offsetHeight <= limit + 1) hi = mid; else lo = mid + 1;
      }
      node.nodeValue = words.slice(lo).join('').replace(/^\s+/, '');
      return;
    }
  }

  // Backstop: if no update arrives for hold_ms (+margin) clear the screen, so a
  // stalled or closed app can never leave stale text on the LED wall.
  function armHold(anyText) {
    if (holdTimer) { clearTimeout(holdTimer); holdTimer = null; }
    if (!anyText || !layout || !layout.hold_ms) return;
    holdTimer = setTimeout(clearAll, layout.hold_ms + 3000);
  }

  function clearAll() {
    Object.keys(frames).forEach(function (id) {
      if (frames[id]) frames[id] = Object.assign({}, frames[id], { stable: '', tentative: '' });
    });
    schedule();
  }

  function push(arr, v) {
    if (v >= 0 && v < 600000) { arr.push(v); if (arr.length > 200) arr.shift(); }
  }

  // ── Transport ─────────────────────────────────────────────────────────
  var es = null;
  var retryDelay = 1000;
  var retryTimer = null;
  var lastSeq = { left: 0, right: 0 };
  function connect() {
    retryTimer = null;
    if (es) { try { es.close(); } catch (err) { /* ignore */ } }
    es = new EventSource('/events' + qs({ screen: screen }));
    es.addEventListener('layout', function (e) {
      try { layout = JSON.parse(e.data); applyLayout(); } catch (err) { /* ignore malformed */ }
    });
    es.addEventListener('caption', function (e) {
      var f;
      try { f = JSON.parse(e.data); } catch (err) { return; }
      if (!f || (f.screen !== 'left' && f.screen !== 'right')) return;
      // Never go back in time within one connection (sequence numbers restart
      // when the app restarts, so the guard is reset on every (re)connect).
      if (f.seq && lastSeq[f.screen] && f.seq < lastSeq[f.screen]) return;
      lastSeq[f.screen] = f.seq || 0;
      frames[f.screen] = f;
      if (f.t_pub_ms) {
        push(stats.transport, Date.now() - f.t_pub_ms);
        pendingMeasure.push(f.t_pub_ms);
      }
      schedule();
    });
    es.onopen = function () {
      retryDelay = 1000;
      lastSeq = { left: 0, right: 0 };
      if (disconnectTimer) { clearTimeout(disconnectTimer); disconnectTimer = null; }
    };
    es.onerror = function () {
      // If the app stays away, clear captions after the hold time instead of
      // freezing stale text on the LED wall.
      if (!disconnectTimer && layout) {
        disconnectTimer = setTimeout(clearAll, Math.max(2000, layout.hold_ms || 5000));
      }
      // EventSource retries by itself (server sends retry: 1000), but Chromium
      // gives up (CLOSED) when a reconnect is refused — e.g. while the app
      // restarts. Keep trying with capped backoff; never reload the page.
      if (es.readyState === 2 && !retryTimer) {
        retryTimer = setTimeout(connect, retryDelay);
        retryDelay = Math.min(retryDelay * 2, 5000);
      }
    };
  }

  // Report overlay latency to the app every 5 s (same machine ⇒ same clock).
  setInterval(function () {
    if (!stats.transport.length && !stats.render.length) return;
    var body = JSON.stringify({ screen: screen, transport: stats.transport, render: stats.render });
    stats.transport = [];
    stats.render = [];
    try {
      fetch('/metrics' + qs(), { method: 'POST', body: body, headers: { 'Content-Type': 'text/plain' }, keepalive: true }).catch(function () {});
    } catch (err) { /* old browser without fetch: metrics are optional */ }
  }, 5000);

  var resizeTimer = null;
  window.addEventListener('resize', function () {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(applyLayout, 100);
  });

  connect();
})();
