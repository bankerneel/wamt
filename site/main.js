(() => {
  const root = document.documentElement;
  const colorScheme = matchMedia('(prefers-color-scheme: dark)');
  const themeToggle = document.querySelector('[data-theme-toggle]');

  function updateThemeButton() {
    const dark = root.dataset.theme ? root.dataset.theme === 'dark' : colorScheme.matches;
    themeToggle.setAttribute('aria-pressed', String(dark));
  }

  themeToggle.addEventListener('click', () => {
    root.dataset.theme = themeToggle.getAttribute('aria-pressed') === 'true' ? 'light' : 'dark';
    updateThemeButton();
  });
  colorScheme.addEventListener('change', updateThemeButton);
  updateThemeButton();
  themeToggle.hidden = false;

  const menuToggle = document.querySelector('[data-menu-toggle]');
  const menu = document.querySelector('[data-menu]');

  function setMenuOpen(open) {
    menuToggle.setAttribute('aria-expanded', String(open));
    menu.toggleAttribute('data-menu-open', open);
  }

  menuToggle.addEventListener('click', () => {
    setMenuOpen(menuToggle.getAttribute('aria-expanded') !== 'true');
  });
  menu.addEventListener('click', event => {
    const link = event.target.closest('a');
    if (!link || menuToggle.getAttribute('aria-expanded') !== 'true') return;
    setMenuOpen(false);
    if (link.hash && link.origin === location.origin) {
      const target = document.getElementById(link.hash.slice(1));
      if (target) {
        target.setAttribute('tabindex', '-1');
        target.focus({ preventScroll: true });
      }
    }
  });
  document.addEventListener('keydown', event => {
    if (event.key !== 'Escape' || event.defaultPrevented || !menu.hasAttribute('data-menu-open')) return;
    setMenuOpen(false);
    menuToggle.focus();
  });
  setMenuOpen(false);
  menuToggle.hidden = false;
  root.setAttribute('data-menu-ready', '');

  const exhibits = document.querySelector('#evidence-dialog');
  if (typeof exhibits?.showModal === 'function') {
    const close = exhibits.querySelector('[data-exhibit-close]');
    const image = exhibits.querySelector('[data-exhibit-image]');
    const title = exhibits.querySelector('[data-exhibit-title]');
    let opener;

    for (const link of document.querySelectorAll('[data-exhibit]')) {
      link.addEventListener('click', event => {
        if (event.button || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
        event.preventDefault();
        const preview = link.querySelector('img');
        opener = link;
        title.textContent = link.dataset.exhibitTitle || preview.alt;
        image.alt = preview.alt;
        image.width = Number(preview.getAttribute('width'));
        image.height = Number(preview.getAttribute('height'));
        image.src = link.href;
        exhibits.showModal();
      });
    }
    close.addEventListener('click', () => exhibits.close());
    exhibits.addEventListener('click', event => {
      if (event.target !== exhibits) return;
      const bounds = exhibits.getBoundingClientRect();
      if (event.clientX < bounds.left || event.clientX > bounds.right ||
          event.clientY < bounds.top || event.clientY > bounds.bottom) exhibits.close();
    });
    exhibits.addEventListener('close', () => opener?.focus({ preventScroll: true }));
  }

  const copy = document.querySelector('[data-copy]');
  const command = document.querySelector('[data-command]');
  const status = document.querySelector('[data-copy-status]');
  copy.addEventListener('click', async () => {
    status.textContent = '';
    try {
      await navigator.clipboard.writeText(command.textContent);
    } catch {
      const selection = getSelection();
      const range = document.createRange();
      range.selectNodeContents(command);
      selection.removeAllRanges();
      selection.addRange(range);
      status.textContent = 'Commands selected. Use your keyboard to copy.';
      return;
    }
    status.textContent = 'Commands copied.';
    document.dispatchEvent(new Event('wamt:copy'));
  });
  copy.hidden = false;

  if (!CSS.supports('animation-timeline: view()') && 'IntersectionObserver' in window) {
    const observer = new IntersectionObserver(entries => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.setAttribute('data-shown', '');
        observer.unobserve(entry.target);
      }
    });
    for (const element of document.querySelectorAll('[data-reveal]')) observer.observe(element);
  }
})();
