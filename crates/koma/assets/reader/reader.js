// Koma-1 reader: turns pages in place (keys, tap thirds, swipes) in the
// book's reading direction, shows the bars, and keeps the position and the
// series' direction in this browser (accounts, M5, will carry them across
// devices). Without it, the page's links do the same a page load at a time.
(() => {
  const body = document.body;
  const book = body.dataset.book;
  const series = body.dataset.series;
  const img = document.getElementById('pg');
  const webp = document.getElementById('pgw');
  const zones = document.querySelectorAll('.zones a');
  const count = document.getElementById('count');
  const store = {
    get: k => { try { return localStorage.getItem('koma:' + k); } catch (e) { return null; } },
    set: (k, v) => { try { localStorage.setItem('koma:' + k, v); } catch (e) {} },
  };
  let dir = store.get('dir:' + series) || body.dataset.dir;
  let n = +body.dataset.n;
  let pages = null;

  // A page's sources: its variants (WebP, then JPEG) and itself.
  const url = (p, w, f) => '/p/' + p.h + (w ? '.' + w + '.v' + pages.v + '.' : '.') + f;
  const set = (p, f) => p.s.filter(([, m]) => m & f).map(([w]) => url(p, w, f & 1 ? 'jpg' : 'webp') + ' ' + w + 'w');
  const show = m => {
    const p = pages.p[m - 1];
    const all = [...set(p, 1), url(p, 0, p.f) + ' ' + p.w + 'w'];
    const sizes = 'min(100vw, ' + (100 * p.w / p.ht).toFixed(1) + 'vh)';
    webp.srcset = [...set(p, 2), url(p, 0, p.f) + ' ' + p.w + 'w'].join(', ');
    webp.sizes = img.sizes = sizes;
    img.srcset = all.join(', ');
    img.src = url(p, 0, p.f);
    img.width = p.w; img.height = p.ht;
    img.alt = 'Page ' + m;
    n = m;
    count.textContent = m + ' / ' + pages.p.length;
    document.title = document.title.replace(/· \d+\/\d+ ·/, '· ' + m + '/' + pages.p.length + ' ·');
    history.replaceState(null, '', '/r/' + book + '/' + m + location.hash);
    store.set('pos:' + book, m);
    links();
  };
  // Forward or back, past either end to the series.
  const turn = by => {
    body.classList.remove('hint');
    if (!pages) return;
    const m = n + by;
    if (m < 1 || m > pages.p.length) { location.href = '/s/' + series; return; }
    show(m);
  };
  // Tap thirds follow the direction: in right-to-left books the left third
  // goes forward.
  const links = () => {
    const fwd = dir === 'rtl' ? 0 : 2;
    zones[fwd].querySelector('span').textContent = (dir === 'rtl' ? '‹ ' : '') + 'next' + (dir === 'rtl' ? '' : ' ›');
    zones[2 - fwd].querySelector('span').textContent = (dir === 'rtl' ? '' : '‹ ') + 'previous' + (dir === 'rtl' ? ' ›' : '');
    for (const a of document.querySelectorAll('a[data-dir]')) {
      if (a.dataset.dir === dir) a.setAttribute('aria-current', 'true'); else a.removeAttribute('aria-current');
    }
  };
  const bars = open => {
    body.classList.toggle('open', open ?? !body.classList.contains('open'));
    if (location.hash) history.replaceState(null, '', location.pathname);
  };

  zones.forEach((a, i) => a.addEventListener('click', e => {
    e.preventDefault();
    body.classList.remove('hint');
    if (i === 1) return bars();
    turn((i === 0) === (dir === 'rtl') ? 1 : -1);
  }));
  for (const a of document.querySelectorAll('a[data-dir]')) {
    a.addEventListener('click', e => {
      e.preventDefault();
      dir = a.dataset.dir;
      store.set('dir:' + series, dir);
      links();
    });
  }
  addEventListener('keydown', e => {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const k = e.key;
    const left = k === 'ArrowLeft' || k === 'h';
    const right = k === 'ArrowRight' || k === 'l';
    if (left || right) turn(left === (dir === 'rtl') ? 1 : -1);
    else if (k === ' ' || k === 'j' || k === 'PageDown') turn(e.shiftKey ? -1 : 1);
    else if (k === 'k' || k === 'PageUp') turn(-1);
    else if (k === 'Enter' || k === 'm') bars();
    else if (k === 'Escape') bars(false);
    else return;
    e.preventDefault();
  });
  // A swipe turns like the tap on the side it starts from.
  let x0 = null;
  addEventListener('touchstart', e => { x0 = e.touches.length === 1 ? e.touches[0].clientX : null; }, { passive: true });
  addEventListener('touchend', e => {
    if (x0 === null) return;
    const dx = e.changedTouches[0].clientX - x0;
    x0 = null;
    if (Math.abs(dx) > 50) turn((dx > 0) === (dir === 'rtl') ? 1 : -1);
  });

  // The tap zones, shown faintly the first time a reader opens.
  if (!store.get('seen')) { body.classList.add('hint'); store.set('seen', 1); }
  links();
  fetch('/b/' + book).then(r => r.ok ? r.json() : null).then(j => { if (j) pages = j; });
})();
