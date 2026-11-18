(() => {
  const names = ['coral', 'lilac', 'blue'];
  const lastBite = 7;
  const motion = matchMedia('(prefers-reduced-motion: reduce)');
  const scenes = Object.fromEntries([...document.querySelectorAll('[data-scene]')].map(scene => [
    scene.dataset.scene,
    { element: scene, sprites: Object.fromEntries([...scene.querySelectorAll('[data-sprite]')].map(sprite => [
      sprite.dataset.sprite, sprite,
    ])) },
  ]));
  const beats = [...document.querySelectorAll('[data-beat]')];
  const stage = document.querySelector('.eating-stage');
  const meter = document.querySelector('[data-plate-meter]');
  const plateLabel = document.querySelector('[data-plate-label]');
  const favicon = document.querySelector('[data-favicon]');
  const originalTitle = document.title;
  const suspects = [...document.querySelectorAll('[data-suspect]')];
  const plateWords = ['whole', 'nibbled', 'bitten', 'bitten', 'half', 'last piece', 'last bite', 'plate empty'];
  const hero = scenes.hero.element;
  const clock = hero.querySelector('[data-cam-clock]');

  let geometry;
  let heroBox;
  let gaze = 1;
  let peekReady = false;
  let clockTimer;
  let paintRequest = 0;
  let celebration = 0;
  let crumbState = -1;

  function setFrame(sprite, frame) {
    const value = String(frame);
    if (sprite.dataset.frame === value) return;
    sprite.style.setProperty('--frame', value);
    sprite.dataset.frame = value;
  }

  function pose(name, frame) {
    for (const blob of names) setFrame(scenes[name].sprites[blob], frame);
  }

  function goToFinale() {
    pose('finale', 11);
    setFrame(scenes.finale.sprites.cookie, lastBite);
  }

  function measure() {
    geometry = beats.map(beat => {
      const { top, height } = beat.getBoundingClientRect();
      return { top: top + scrollY, length: Math.min(innerHeight * 0.25, height * 0.65) };
    });
    const rect = hero.getBoundingClientRect();
    heroBox = { x: rect.left + rect.width / 2, y: rect.top + scrollY + rect.height / 2 };
    schedulePaint();
  }

  function storyPosition() {
    const reader = scrollY + innerHeight * 0.64;
    let bite = 0;
    while (bite < lastBite && reader >= geometry[bite + 1].top) bite++;
    const { top, length } = geometry[bite];
    const progress = bite ? Math.min(1, Math.max(0, (reader - top) / length)) : 0;
    return { bite, progress };
  }

  function paintHero(reduced) {
    if (reduced) {
      hero.dataset.pose = 'still';
      pose('hero', 9);
    } else if (scrollY > 12) {
      const step = Math.min(2, Math.floor((scrollY - 13) / 36));
      hero.dataset.pose = 'sneak';
      hero.style.setProperty('--step', step);
      pose('hero', 6 + step);
    } else {
      hero.dataset.pose = peekReady ? 'peek' : 'hidden';
      pose('hero', peekReady ? gaze : 0);
    }
    setFrame(scenes.hero.sprites.cookie, reduced ? 2 : 1);
  }

  function tickClock() {
    const time = new Date().toTimeString().slice(0, 8);
    clock.dateTime = time;
    clock.textContent = time;
  }

  function paint() {
    paintRequest = 0;
    const reduced = motion.matches;
    const { bite, progress } = storyPosition();
    const eating = !reduced && bite > 0 && bite < lastBite && progress > 0 && progress < 1;
    const state = reduced ? lastBite : Math.max(0, bite - (progress < 0.32 ? 1 : 0));
    const chomp = eating ? 4 + Math.min(5, Math.floor(progress * 6)) : 0;
    const active = names[(bite - 1) % names.length];

    for (const name of names) {
      setFrame(scenes.stage.sprites[name], reduced ? 11 : name === active ? chomp : 0);
    }
    setFrame(scenes.stage.sprites.cookie, state);
    const burst = scenes.stage.sprites.burst;
    burst.hidden = !eating || progress < 0.32 || progress >= 0.58;
    if (!burst.hidden) setFrame(burst, 8 + Math.min(3, Math.floor((progress - 0.32) * 16)));
    stage.dataset.storyBeat = String(bite);
    stage.dataset.state = String(state);
    stage.dataset.biteFrame = String(chomp);
    meter.value = lastBite - state;
    if (plateLabel.textContent !== plateWords[state]) plateLabel.textContent = plateWords[state];

    const href = '/img/sprites/favicon-' + state + '.png';
    if (favicon.getAttribute('href') !== href) favicon.setAttribute('href', href);

    paintHero(reduced);

    if (state !== crumbState) {
      crumbState = state;
      document.dispatchEvent(new CustomEvent('wamt:crumbs', { detail: { state, bite, progress, reduced } }));
    }
  }

  function schedulePaint() {
    if (!paintRequest) paintRequest = requestAnimationFrame(paint);
  }

  document.addEventListener('pointermove', event => {
    if (motion.matches || scrollY > 12 || event.pointerType === 'touch') return;
    const dx = event.clientX - heroBox.x;
    const dy = event.clientY - heroBox.y + scrollY;
    let direction = 1;
    if (Math.max(Math.abs(dx), Math.abs(dy)) > 48) {
      direction = Math.abs(dx) > Math.abs(dy) ? (dx < 0 ? 2 : 3) : (dy < 0 ? 4 : 5);
    }
    if (direction !== gaze) {
      gaze = direction;
      schedulePaint();
    }
  }, { passive: true });
  document.documentElement.addEventListener('pointerleave', () => {
    gaze = 1;
    schedulePaint();
  });

  for (const suspect of suspects) {
    const sprite = suspect.querySelector('[data-sprite]');
    function react() {
      const startled = suspect.matches(':hover, :focus');
      setFrame(sprite, motion.matches ? 11 : startled ? 10 : 0);
    }
    suspect.addEventListener('pointerenter', react);
    suspect.addEventListener('pointerleave', react);
    suspect.addEventListener('focus', react);
    suspect.addEventListener('blur', react);
    motion.addEventListener('change', react);
    react();
  }

  function stopCelebrating() {
    cancelAnimationFrame(celebration);
    celebration = 0;
    goToFinale();
  }

  document.addEventListener('wamt:copy', () => {
    if (motion.matches || document.hidden || celebration) return;
    const start = performance.now();
    function celebrate(now) {
      const frame = Math.floor(Math.max(0, now - start) / 100);
      if (frame >= 6) {
        stopCelebrating();
        return;
      }
      pose('finale', 4 + frame);
      celebration = requestAnimationFrame(celebrate);
    }
    celebration = requestAnimationFrame(celebrate);
  });

  document.addEventListener('visibilitychange', () => {
    document.title = document.hidden ? 'there is still cookie left' : originalTitle;
    if (document.hidden && celebration) stopCelebrating();
  });
  motion.addEventListener('change', () => {
    if (motion.matches) stopCelebrating();
    crumbState = -1;
    schedulePaint();
  });
  addEventListener('scroll', schedulePaint, { passive: true });
  addEventListener('resize', measure);
  addEventListener('load', measure);
  new ResizeObserver(measure).observe(document.body);
  new IntersectionObserver(([entry]) => {
    clearInterval(clockTimer);
    if (!entry.isIntersecting) return;
    tickClock();
    clock.hidden = false;
    clockTimer = setInterval(tickClock, 1000);
  }).observe(hero);
  paintHero(motion.matches);
  setTimeout(() => {
    peekReady = true;
    schedulePaint();
  }, 420);
  document.documentElement.setAttribute('data-reactions', '');
  goToFinale();
  measure();
})();
