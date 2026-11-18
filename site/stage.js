/*
 * wamt landing scroll story stage controller.
 * Classic deferred script. Declarative layout and masking come from the lead CSS.
 * Frame ranges and sheet sizes follow the sprite contract in the landing brief.
 */
(function () {
  'use strict';

  var BLOB_WIDTH = 136;
  var COOKIE_WIDTH = 104;
  var PLATE_WIDTH = 0;
  var BLOBS = ['coral', 'lilac', 'blue'];
  var MAX_BEAT = 7;
  var HIDDEN_TITLE = 'there is still cookie left';

  var heroScene = document.querySelector('.scene[data-scene="hero"]');
  var stageScene = document.querySelector('.scene[data-scene="stage"]');
  var finaleScene = document.querySelector('.scene[data-scene="finale"]');
  var eatingStage = document.querySelector('.eating-stage');
  var meter = document.querySelector('[data-plate-meter]');
  var favicon = document.querySelector('[data-favicon]');
  var beatEls = document.querySelectorAll('[data-beat]');
  var suspectEls = document.querySelectorAll('[data-suspect]');

  var heroSprites = null;
  var stageSprites = null;
  var finaleSprites = null;

  var beatGroups = [];
  var heroHeight = 1;
  var heroCenterX = 0;
  var heroCenterY = 0;

  var reduced = false;
  var visible = { hero: false, stage: false, finale: false };
  var heroSneaked = false;
  var pointerDir = 12;
  var celebrating = false;
  var rafPending = false;
  var lastKey = '';
  var originalTitle = '';

  function clamp(value, lo, hi) {
    return value < lo ? lo : (value > hi ? hi : value);
  }

  function clampInt(value, lo, hi) {
    return Math.round(clamp(value, lo, hi));
  }

  function setAttr(el, name, value) {
    if (el && el.getAttribute(name) !== value) el.setAttribute(name, value);
  }

  function widthFor(type) {
    if (type === 'cookie') return COOKIE_WIDTH;
    if (type === 'plate') return PLATE_WIDTH;
    return BLOB_WIDTH;
  }

  function maxFor(type) {
    if (type === 'cookie') return 7;
    if (type === 'plate') return 0;
    return 19;
  }

  function setFrame(sprite, type, frame) {
    if (!sprite) return;
    var f = clampInt(frame, 0, maxFor(type));
    setAttr(sprite, 'data-frame', String(f));
    var x = (-f * widthFor(type)) + 'px';
    if (sprite.style.backgroundPositionX !== x) sprite.style.backgroundPositionX = x;
  }

  function spritesIn(scene) {
    var out = { coral: null, lilac: null, blue: null, plate: null, cookie: null };
    if (!scene) return out;
    var list = scene.querySelectorAll('.sprite[data-sprite]');
    for (var i = 0; i < list.length; i++) {
      var type = list[i].getAttribute('data-sprite');
      if (Object.prototype.hasOwnProperty.call(out, type)) out[type] = list[i];
    }
    return out;
  }

  function readGeometry() {
    var found = false;
    beatGroups = [];
    for (var n = 0; n <= MAX_BEAT; n++) beatGroups.push(null);

    for (var i = 0; i < beatEls.length; i++) {
      var raw = parseInt(beatEls[i].getAttribute('data-beat'), 10);
      if (isNaN(raw)) continue;
      var beat = clamp(raw, 0, MAX_BEAT);
      var rect = beatEls[i].getBoundingClientRect();
      var top = rect.top + window.pageYOffset;
      var bottom = rect.bottom + window.pageYOffset;
      if (!beatGroups[beat]) beatGroups[beat] = { top: top, bottom: bottom };
      else {
        if (top < beatGroups[beat].top) beatGroups[beat].top = top;
        if (bottom > beatGroups[beat].bottom) beatGroups[beat].bottom = bottom;
      }
      found = true;
    }

    var docHeight = Math.max(
      document.documentElement.scrollHeight || 0,
      document.body.scrollHeight || 0
    );
    var scrollMax = Math.max(1, docHeight - window.innerHeight);

    if (!found) {
      var span = scrollMax / 8;
      for (var s = 0; s <= MAX_BEAT; s++) {
        beatGroups[s] = { top: s * span, bottom: (s + 1) * span };
      }
    } else {
      for (var m = 0; m <= MAX_BEAT; m++) {
        if (beatGroups[m]) continue;
        var prev = null;
        var next = null;
        for (var a = m - 1; a >= 0; a--) {
          if (beatGroups[a]) { prev = beatGroups[a]; break; }
        }
        for (var b = m + 1; b <= MAX_BEAT; b++) {
          if (beatGroups[b]) { next = beatGroups[b]; break; }
        }
        if (prev && next && next.top > prev.bottom) {
          beatGroups[m] = { top: prev.bottom, bottom: next.top };
        } else if (prev) {
          beatGroups[m] = { top: prev.bottom, bottom: prev.bottom + 1 };
        } else if (next) {
          beatGroups[m] = { top: next.top - 1, bottom: next.top };
        } else {
          beatGroups[m] = { top: 0, bottom: 1 };
        }
      }
    }

    if (heroScene) {
      var hr = heroScene.getBoundingClientRect();
      heroHeight = Math.max(1, hr.height);
      heroCenterX = hr.left + hr.width / 2;
      heroCenterY = hr.top + hr.height / 2 + window.pageYOffset;
    } else {
      heroHeight = Math.max(1, window.innerHeight);
      heroCenterX = window.innerWidth / 2;
      heroCenterY = window.innerHeight / 2;
    }
  }

  function storyFromScroll() {
    var anchor = window.pageYOffset + window.innerHeight * 0.5;
    var beat = 0;
    var progress = 0;
    for (var i = 0; i <= MAX_BEAT; i++) {
      var group = beatGroups[i];
      if (!group) continue;
      var start = group.top;
      var end = group.bottom > group.top ? group.bottom : group.top + 1;
      if (anchor >= start && anchor < end) {
        beat = i;
        progress = clamp((anchor - start) / (end - start), 0, 1);
        break;
      }
      if (anchor >= end) {
        beat = i;
        progress = 1;
      }
    }
    return { beat: beat, progress: progress };
  }

  function cookieFrameFor(beat, progress) {
    return clamp(Math.floor((beat - 1) + progress + 0.5), 0, MAX_BEAT);
  }

  function activeBlobFor(beat) {
    return BLOBS[((beat - 1) % BLOBS.length + BLOBS.length) % BLOBS.length];
  }

  function chompFrameFor(progress) {
    return clamp(4 + Math.floor(progress * 6), 4, 9);
  }

  function idleFrameFor(y) {
    return Math.floor(Math.max(0, y) / 720) % 4;
  }

  function setFavicon(frame) {
    if (!favicon) return;
    var f = clampInt(frame, 0, 7);
    var href = '/img/sprites/favicon-' + f + '.png';
    setAttr(favicon, 'href', href);
  }

  function setFinaleStatic() {
    if (!finaleSprites) return;
    for (var i = 0; i < BLOBS.length; i++) setFrame(finaleSprites[BLOBS[i]], BLOBS[i], 11);
    setFrame(finaleSprites.cookie, 'cookie', 7);
    setFrame(finaleSprites.plate, 'plate', 0);
  }

  function emit(state, bite, progress, isReduced) {
    var key = state + ':' + bite + ':' + progress + ':' + (isReduced ? 'r' : 'n');
    if (key === lastKey) return;
    lastKey = key;
    document.dispatchEvent(new CustomEvent('wamt:crumbs', {
      detail: {
        state: state,
        bite: bite,
        progress: progress,
        reduced: isReduced
      }
    }));
  }

  function setMeter(value) {
    if (!meter) return;
    setAttr(meter, 'min', '0');
    setAttr(meter, 'max', '7');
    setAttr(meter, 'value', String(value));
    if (String(meter.value) !== String(value)) meter.value = value;
  }

  function applyScroll() {
    rafPending = false;
    if (reduced) return;

    var story = storyFromScroll();
    var beat = story.beat;
    var progress = story.progress;
    var state = cookieFrameFor(beat, progress);
    var bite = beat;
    var active = activeBlobFor(beat);
    var chomp = chompFrameFor(progress);
    var idle = idleFrameFor(window.pageYOffset);

    if (stageSprites) {
      for (var i = 0; i < BLOBS.length; i++) {
        var name = BLOBS[i];
        var frame = (visible.stage && name === active) ? chomp : idle;
        setFrame(stageSprites[name], name, frame);
      }
      setFrame(stageSprites.cookie, 'cookie', state);
      setFrame(stageSprites.plate, 'plate', 0);
    }

    var host = eatingStage || stageScene;
    if (host) {
      setAttr(host, 'data-beat', String(beat));
      setAttr(host, 'data-state', String(state));
      setAttr(host, 'data-bite-frame', String(visible.stage && active ? chomp : 0));
    }

    setMeter(state);
    setFavicon(state);

    if (heroScene) {
      if (window.pageYOffset > 8) heroSneaked = true;
      setAttr(heroScene, 'data-pose', heroSneaked ? 'sneak' : 'peek');
      if (heroSprites) {
        if (visible.hero) {
          if (heroSneaked) {
            var sp = clamp(window.pageYOffset / Math.max(1, heroHeight * 0.6), 0, 1);
            var sneakFrame = clamp(17 + Math.floor(sp * 3), 17, 19);
            for (var j = 0; j < BLOBS.length; j++) setFrame(heroSprites[BLOBS[j]], BLOBS[j], sneakFrame);
          } else {
            for (var k = 0; k < BLOBS.length; k++) setFrame(heroSprites[BLOBS[k]], BLOBS[k], pointerDir);
          }
        }
        setFrame(heroSprites.cookie, 'cookie', 0);
        setFrame(heroSprites.plate, 'plate', 0);
      }
    }

    if (!celebrating) setFinaleStatic();

    emit(state, bite, Math.round(progress * 8) / 8, false);
  }

  function applyReduced() {
    if (heroScene) {
      setAttr(heroScene, 'data-pose', 'peek');
      if (heroSprites) {
        for (var i = 0; i < BLOBS.length; i++) setFrame(heroSprites[BLOBS[i]], BLOBS[i], 12);
        setFrame(heroSprites.cookie, 'cookie', 0);
        setFrame(heroSprites.plate, 'plate', 0);
      }
    }
    if (stageSprites) {
      for (var j = 0; j < BLOBS.length; j++) setFrame(stageSprites[BLOBS[j]], BLOBS[j], 11);
      setFrame(stageSprites.cookie, 'cookie', 7);
      setFrame(stageSprites.plate, 'plate', 0);
    }
    var host = eatingStage || stageScene;
    if (host) {
      setAttr(host, 'data-beat', '7');
      setAttr(host, 'data-state', '7');
      setAttr(host, 'data-bite-frame', '11');
    }
    setMeter(7);
    setFinaleStatic();
    for (var s = 0; s < suspectEls.length; s++) {
      var sprite = suspectEls[s].querySelector('.sprite[data-sprite]');
      if (sprite) setFrame(sprite, 'coral', 11);
    }
    setFavicon(7);
    emit(7, 7, 1, true);
  }

  function schedule() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(applyScroll);
  }

  function onScroll() { schedule(); }

  function onResize() {
    readGeometry();
    schedule();
  }

  function onPointerMove(event) {
    if (reduced || !heroScene || !visible.hero) return;
    var dx = event.clientX - heroCenterX;
    var dy = event.clientY - (heroCenterY - window.pageYOffset);
    var ax = Math.abs(dx);
    var ay = Math.abs(dy);
    var threshold = 60;
    var dir;
    if (ax < threshold && ay < threshold) dir = 12;
    else if (ax >= ay) dir = dx < 0 ? 13 : 14;
    else dir = dy < 0 ? 15 : 16;
    if (dir !== pointerDir) {
      pointerDir = dir;
      schedule();
    }
  }

  function onVisibility() {
    if (document.hidden) {
      if (document.title !== HIDDEN_TITLE) document.title = HIDDEN_TITLE;
    } else if (document.title === HIDDEN_TITLE) {
      document.title = originalTitle;
    }
  }

  function onCopy() {
    if (reduced || celebrating || !finaleSprites) return;
    celebrating = true;
    var start = null;
    function step(ts) {
      if (start === null) start = ts;
      var t = (ts - start) / 560;
      if (t >= 1) {
        setFinaleStatic();
        celebrating = false;
        return;
      }
      var frame = 4 + Math.min(5, Math.floor(t * 6));
      for (var i = 0; i < BLOBS.length; i++) setFrame(finaleSprites[BLOBS[i]], BLOBS[i], frame);
      requestAnimationFrame(step);
    }
    requestAnimationFrame(step);
  }

  function onMedia(event) {
    reduced = !!event.matches;
    if (reduced) {
      celebrating = false;
      applyReduced();
    } else {
      lastKey = '';
      heroSneaked = false;
      readGeometry();
      schedule();
    }
  }

  function setupObserver() {
    if (!('IntersectionObserver' in window)) {
      visible.hero = visible.stage = visible.finale = true;
      return;
    }
    var io = new IntersectionObserver(function (entries) {
      for (var i = 0; i < entries.length; i++) {
        var name = entries[i].target.getAttribute('data-scene');
        if (name === 'hero' || name === 'stage' || name === 'finale') {
          visible[name] = entries[i].isIntersecting;
        }
      }
      schedule();
    }, { rootMargin: '10% 0px' });
    if (heroScene) io.observe(heroScene);
    if (stageScene) io.observe(stageScene);
    if (finaleScene) io.observe(finaleScene);
  }

  function setupSuspects() {
    for (var i = 0; i < suspectEls.length; i++) {
      (function (el) {
        var sprite = el.querySelector('.sprite[data-sprite]');
        if (!sprite) return;
        var type = el.getAttribute('data-suspect');
        if (BLOBS.indexOf(type) < 0) type = 'coral';
        function enter() { setFrame(sprite, type, reduced ? 11 : 10); }
        function leave() { setFrame(sprite, type, reduced ? 11 : 0); }
        el.addEventListener('mouseenter', enter);
        el.addEventListener('mouseleave', leave);
        el.addEventListener('focus', enter);
        el.addEventListener('blur', leave);
      })(suspectEls[i]);
    }
  }

  function init() {
    heroSprites = spritesIn(heroScene);
    stageSprites = spritesIn(stageScene);
    finaleSprites = spritesIn(finaleScene);
    originalTitle = document.title;

    readGeometry();
    setupObserver();
    setupSuspects();

    window.addEventListener('scroll', onScroll, { passive: true });
    window.addEventListener('resize', onResize);
    window.addEventListener('load', onResize);
    document.addEventListener('visibilitychange', onVisibility);
    document.addEventListener('wamt:copy', onCopy);
    if (heroScene) document.addEventListener('pointermove', onPointerMove, { passive: true });

    var mq = window.matchMedia ? window.matchMedia('(prefers-reduced-motion: reduce)') : null;
    if (mq) {
      reduced = !!mq.matches;
      if (mq.addEventListener) mq.addEventListener('change', onMedia);
      else if (mq.addListener) mq.addListener(onMedia);
    }

    if (reduced) {
      applyReduced();
    } else {
      setAttr(heroScene, 'data-pose', 'peek');
      setFinaleStatic();
      schedule();
    }
  }

  init();
})();
