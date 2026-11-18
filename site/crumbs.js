/* wamt crumbs: one decorative canvas of pixel crumbs for the landing scroll story.
   Contract: listens for the document event wamt:crumbs with detail
   {state: 0..7, bite: 1..7, progress: 0..1, reduced: boolean}.
   The pile is derived from state alone, so scroll back is exact and deep links settle
   straight into place. Lead CSS positions the canvas through .crumb-canvas. */
(function () {
  'use strict';

  var FRAME = 12;
  var FRAMES = 6;
  var PER_BITE = 6;
  var MAX = 48;
  var SHEET = '/img/sprites/crumbs.png';
  var EVENT = 'wamt:crumbs';

  var canvas = null;
  var ctx = null;
  var img = null;
  var imgReady = false;
  var hosts = [];
  var metrics = [];
  var io = null;
  var ro = null;
  var raf = 0;
  var mq = null;
  var active = null;
  var state = -1;
  var lastBite = 0;
  var bits = [];
  var spill = [];
  var W = 1;
  var H = 1;
  var srcX = 0;
  var srcY = 0;

  function clamp(n, lo, hi) { return n < lo ? lo : (n > hi ? hi : n); }

  function hash(n) {
    n = (n ^ 61) ^ (n >>> 16);
    n = n + (n << 3);
    n = n ^ (n >>> 4);
    n = Math.imul(n, 0x27d4eb2d);
    n = n ^ (n >>> 15);
    return (n >>> 0) / 4294967296;
  }

  function reduced() { return !!(mq && mq.matches); }

  function findHosts() {
    var list = document.querySelectorAll('.scene[data-scene="stage"], .scene[data-scene="finale"]');
    hosts = [];
    metrics = [];
    for (var i = 0; i < list.length; i++) {
      hosts.push(list[i]);
      metrics.push({ el: list[i], area: 0, visible: false });
    }
  }

  function init() {
    if (canvas) return;
    findHosts();
    if (!hosts.length) {
      var obs = null;
      var onAdd = function () {
        findHosts();
        if (hosts.length) { if (obs) obs.disconnect(); begin(); }
      };
      if ('MutationObserver' in window) obs = new MutationObserver(onAdd);
      if (obs) obs.observe(document.documentElement, { childList: true, subtree: true });
      window.addEventListener('load', function () { findHosts(); if (hosts.length) begin(); });
      return;
    }
    begin();
  }

  function begin() {
    if (canvas) return;
    canvas = document.createElement('canvas');
    canvas.className = 'crumb-canvas';
    canvas.setAttribute('aria-hidden', 'true');
    canvas.style.pointerEvents = 'none';
    ctx = canvas.getContext('2d');
    if (!ctx) { canvas = null; return; }
    ctx.imageSmoothingEnabled = false;
    if (window.matchMedia) mq = window.matchMedia('(prefers-reduced-motion: reduce)');
    if (mq && mq.addEventListener) mq.addEventListener('change', onMotion);
    else if (mq && mq.addListener) mq.addListener(onMotion);
    document.addEventListener(EVENT, onCrumbs);
    document.addEventListener('visibilitychange', onVis);
    window.addEventListener('resize', onResize);
    if ('IntersectionObserver' in window) {
      io = new IntersectionObserver(onIntersect, { threshold: [0, 0.05, 0.25, 0.5, 0.75, 1] });
      for (var i = 0; i < hosts.length; i++) io.observe(hosts[i]);
    }
    if ('ResizeObserver' in window) {
      ro = new ResizeObserver(onResize);
      for (var j = 0; j < hosts.length; j++) ro.observe(hosts[j]);
    }
    loadImage();
    chooseHost();
    requestPaint();
  }

  function loadImage() {
    if (img) return;
    img = new Image();
    img.onload = function () { imgReady = true; requestPaint(); };
    img.onerror = function () { imgReady = false; };
    img.src = SHEET;
    if (img.decode) {
      img.decode().then(function () { imgReady = true; requestPaint(); }).catch(function () {});
    }
  }

  function onIntersect(entries) {
    for (var e = 0; e < entries.length; e++) {
      var en = entries[e];
      for (var i = 0; i < metrics.length; i++) {
        if (metrics[i].el === en.target) {
          metrics[i].area = en.intersectionRect.width * en.intersectionRect.height;
          metrics[i].visible = en.isIntersecting;
        }
      }
    }
    chooseHost();
  }

  function anyVisible() {
    if (document.hidden) return false;
    for (var i = 0; i < hosts.length; i++) {
      var m = metrics[i];
      if (m && m.visible && m.area > 0) return true;
      if (!io) {
        var r = hosts[i].getBoundingClientRect();
        if (r.bottom > 0 && r.top < window.innerHeight && r.right > 0 && r.left < window.innerWidth) return true;
      }
    }
    return false;
  }

  function chooseHost() {
    if (!hosts.length || !canvas) return;
    var best = null;
    var bestArea = -1;
    for (var i = 0; i < hosts.length; i++) {
      var area = 0;
      var m = metrics[i];
      if (m && m.visible) area = m.area;
      if (!io || !m) {
        var r = hosts[i].getBoundingClientRect();
        var ix = Math.max(0, Math.min(r.right, window.innerWidth) - Math.max(r.left, 0));
        var iy = Math.max(0, Math.min(r.bottom, window.innerHeight) - Math.max(r.top, 0));
        area = ix * iy;
      }
      if (area > bestArea) { bestArea = area; best = hosts[i]; }
    }
    if (!best) best = hosts[0];
    if (best === active) return;
    active = best;
    if (canvas.parentNode !== active) {
      if (canvas.parentNode) canvas.parentNode.removeAttribute('data-crumbs-host');
      active.appendChild(canvas);
    }
    sizeCanvas();
    if (isFinale()) syncSpill(false);
    else spill = [];
    layout();
    requestPaint();
  }

  function isFinale() { return !!(active && active.getAttribute('data-scene') === 'finale'); }

  function sizeCanvas() {
    if (!active || !ctx) return;
    var r = active.getBoundingClientRect();
    W = Math.max(1, Math.round(r.width));
    H = Math.max(1, Math.round(r.height));
    if (canvas.width !== W) canvas.width = W;
    if (canvas.height !== H) canvas.height = H;
    canvas.style.width = W + 'px';
    canvas.style.height = H + 'px';
    ctx.imageSmoothingEnabled = false;
    updateSource();
  }

  function updateSource() {
    if (!active) return;
    var hr = active.getBoundingClientRect();
    var cookie = active.querySelector('.sprite[data-sprite="cookie"]');
    if (cookie) {
      var cr = cookie.getBoundingClientRect();
      srcX = clamp(Math.round(cr.left + cr.width / 2 - hr.left), FRAME / 2, W - FRAME / 2);
      srcY = clamp(Math.round(cr.top + cr.height / 2 - hr.top), FRAME / 2, H - FRAME / 2);
    } else {
      srcX = Math.round(W / 2);
      srcY = Math.round(H * 0.55);
    }
  }

  function heapRest(k) {
    var slots = Math.min(17, Math.max(1, Math.floor(W / 9) - 2));
    var row = Math.floor(k / slots);
    var idx = k % slots;
    var ground = Math.max(FRAME, H - 10);
    var x = srcX + Math.ceil(idx / 2) * (idx % 2 ? 9 : -9) + (hash(k * 7 + 1) * 6 - 3);
    var y = ground - row * 5 - (k % 3);
    return {
      x: clamp(Math.round(x), FRAME / 2, W - FRAME / 2),
      y: clamp(Math.round(y), FRAME / 2, H - FRAME / 2)
    };
  }

  function spillRest(k) {
    var spread = Math.min(140, W * 0.36);
    var x = W / 2 + (hash(k * 13 + 5) * 2 - 1) * spread;
    var y = (H - FRAME) - (k % 3) * (FRAME - 3);
    return {
      x: clamp(Math.round(x), FRAME / 2, W - FRAME / 2),
      y: clamp(Math.round(y), FRAME / 2, H - FRAME / 2)
    };
  }

  function layout() {
    for (var i = 0; i < bits.length; i++) {
      var p = bits[i];
      var r = heapRest(p.k);
      p.rx = r.x; p.ry = r.y;
      if (p.settled) { p.x = r.x; p.y = r.y; }
    }
    for (var j = 0; j < spill.length; j++) {
      var q = spill[j];
      var s = spillRest(q.k);
      q.rx = s.x; q.ry = s.y;
      if (q.settled) { q.x = s.x; q.y = s.y; }
    }
  }

  function addGroup(g, animate) {
    for (var i = 0; i < PER_BITE; i++) {
      var k = (g - 1) * PER_BITE + i;
      var rest = heapRest(k);
      bits.push({
        k: k, group: g, frame: (k + (lastBite || g)) % FRAMES,
        x: animate ? srcX : rest.x,
        y: animate ? srcY : rest.y,
        rx: rest.x, ry: rest.y,
        vx: animate ? (rest.x - srcX) * 0.052 : 0,
        vy: animate ? -1.2 - hash(k * 5 + 7) * 1.4 : 0,
        settled: !animate,
        bounces: 2
      });
    }
  }

  function removeAbove(s) {
    var keep = [];
    for (var i = 0; i < bits.length; i++) {
      if (bits[i].group <= s) keep.push(bits[i]);
    }
    bits = keep;
  }

  function syncSpill(animate) {
    var n = state > 0 ? Math.min(PER_BITE, state + 1) : 0;
    if (spill.length === n && !animate) { layout(); return; }
    spill = [];
    for (var i = 0; i < n; i++) {
      var s = spillRest(i);
      spill.push({
        k: i, frame: (i + (state || 1)) % FRAMES,
        x: animate ? Math.round(W / 2) : s.x,
        y: animate ? Math.round(H * 0.35) : s.y,
        rx: s.x, ry: s.y,
        vx: animate ? (hash(i * 17 + 3) * 2 - 1) * 1.4 : 0,
        vy: animate ? 0.5 : 0,
        settled: !animate,
        bounces: 2
      });
    }
  }

  function settleAll() {
    var arrs = [bits, spill];
    for (var a = 0; a < arrs.length; a++) {
      for (var i = 0; i < arrs[a].length; i++) {
        var p = arrs[a][i];
        p.settled = true; p.vx = 0; p.vy = 0; p.x = p.rx; p.y = p.ry;
      }
    }
  }

  function applyState(s, eventReduced) {
    if (s === state) return;
    var prev = state;
    var first = prev < 0;
    state = s;
    if (s < prev) {
      removeAbove(s);
    } else {
      var animate = !eventReduced && !reduced() && !(first && s > 1);
      var from = prev < 0 ? 1 : prev + 1;
      for (var g = from; g <= s; g++) addGroup(g, animate);
      if (bits.length > MAX) bits.splice(0, bits.length - MAX);
    }
    if (isFinale()) syncSpill(!eventReduced && !reduced() && !first && s > prev);
    else spill = [];
    layout();
    if (eventReduced || reduced()) settleAll();
    requestPaint();
  }

  function onCrumbs(e) {
    var d = (e && e.detail) ? e.detail : {};
    var s = clamp(Math.round(Number(d.state) || 0), 0, 7);
    var b = clamp(Math.round(Number(d.bite) || 0), 0, 7);
    if (b > 0) lastBite = b;
    applyState(s, d.reduced === true);
  }

  function onMotion() {
    if (reduced()) { settleAll(); stopLoop(); }
    requestPaint();
  }

  function onVis() {
    if (document.hidden) stopLoop();
    else requestPaint();
  }

  function onResize() {
    if (!active) return;
    sizeCanvas();
    layout();
    requestPaint();
  }

  function activeSet() { return isFinale() ? spill : bits; }

  function hasActive() {
    var arr = activeSet();
    for (var i = 0; i < arr.length; i++) if (!arr[i].settled) return true;
    return false;
  }

  function stopLoop() {
    if (raf) { cancelAnimationFrame(raf); raf = 0; }
  }

  function requestPaint() {
    if (!imgReady || !canvas) return;
    if (document.hidden || !anyVisible()) { stopLoop(); return; }
    if (reduced() || !hasActive()) { stopLoop(); paint(); return; }
    if (!raf) raf = requestAnimationFrame(tick);
  }

  function tick() {
    raf = 0;
    if (document.hidden || !anyVisible() || reduced()) { stopLoop(); paint(); return; }
    var moving = step();
    paint();
    if (moving) raf = requestAnimationFrame(tick);
  }

  function step() {
    var arr = activeSet();
    var moving = false;
    for (var i = 0; i < arr.length; i++) {
      var p = arr[i];
      if (p.settled) continue;
      p.vy += 0.45;
      p.x += p.vx;
      p.y += p.vy;
      if (p.y >= p.ry) {
        p.y = p.ry;
        if (p.bounces > 0 && Math.abs(p.vy) > 0.8) {
          p.vy = -p.vy * 0.34;
          p.vx *= 0.55;
          p.bounces--;
          moving = true;
        } else {
          p.vy = 0; p.vx = 0; p.x = p.rx; p.y = p.ry; p.settled = true;
        }
      } else {
        if (p.x < FRAME / 2) { p.x = FRAME / 2; p.vx = Math.abs(p.vx) * 0.5; }
        if (p.x > W - FRAME / 2) { p.x = W - FRAME / 2; p.vx = -Math.abs(p.vx) * 0.5; }
        moving = true;
      }
    }
    return moving;
  }

  function paint() {
    if (!ctx || !imgReady || !canvas.width) return;
    ctx.clearRect(0, 0, W, H);
    ctx.imageSmoothingEnabled = false;
    var arr = activeSet();
    for (var i = 0; i < arr.length; i++) {
      var p = arr[i];
      ctx.drawImage(img, p.frame * FRAME, 0, FRAME, FRAME,
        Math.round(p.x) - FRAME / 2, Math.round(p.y) - FRAME / 2, FRAME, FRAME);
    }
    if (!canvas.getAttribute('data-crumbs-painted')) {
      canvas.setAttribute('data-crumbs-painted', '1');
    }
    if (active) active.setAttribute('data-crumbs-host', '1');
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init, { once: true });
  } else {
    init();
  }
})();
