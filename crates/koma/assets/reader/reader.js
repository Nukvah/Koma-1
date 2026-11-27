// Koma-1 reader: turns pages in place (keys, tap thirds, swipes) in the
// book's reading direction, with the next pages fetched and decoded ahead
// as far as the reading speed needs; shows the bars; keeps the position and
// the series' direction in this browser (accounts, M5, will carry them
// across devices). Without it, the page's links do the same a page load at
// a time.
(() => {
  const body = document.body;
  const book = body.dataset.book;
  const series = body.dataset.series;
  const zones = document.querySelectorAll('.zones a');
  const count = document.getElementById('count');
  const store = {
    get: k => { try { return localStorage.getItem('koma:' + k); } catch (e) { return null; } },
    set: (k, v) => { try { localStorage.setItem('koma:' + k, v); } catch (e) {} },
  };
  let dir = store.get('dir:' + series) || body.dataset.dir;
  let n = +body.dataset.n;
  let pages = null;

  // A page's sources: its variants in one format (WebP where the browser
  // takes it, else JPEG) and itself. With Save-Data, the 720 one only.
  const webp = (() => {
    try { return document.createElement('canvas').toDataURL('image/webp').startsWith('data:image/webp'); } catch (e) { return false; }
  })();
  const save = !!(navigator.connection && navigator.connection.saveData);
  const url = (p, w, f) => '/p/' + p.h + (w ? '.' + w + '.v' + pages.v + '.' : '.') + f;
  const srcset = p => {
    const f = webp ? 2 : 1;
    const vs = p.s.filter(([w, m]) => m & f && (!save || w === 720));
    const set = vs.map(([w]) => url(p, w, webp ? 'webp' : 'jpg') + ' ' + w + 'w');
    if (!save || !vs.length) set.push(url(p, 0, p.f) + ' ' + p.w + 'w');
    return set.join(', ');
  };
  // Pages made ahead: each an image of its own, fetched at low priority and
  // decoded before it is needed, so a turn only swaps it in.
  const made = new Map();
  const make = (m, low) => {
    let e = made.get(m);
    if (!e) {
      const p = pages.p[m - 1];
      const im = new Image();
      im.fetchPriority = low ? 'low' : 'high';
      im.sizes = 'min(100vw, ' + (100 * p.w / p.ht).toFixed(1) + 'vh)';
      im.srcset = srcset(p);
      im.width = p.w; im.height = p.ht;
      im.alt = 'Page ' + m;
      im.id = 'pg';
      e = { im, ready: im.decode().catch(() => {}) };
      made.set(m, e);
    }
    return e;
  };
  // Reading speed: the last few times between turns (pauses over a minute
  // left out). Ahead: enough pages for two seconds, 2 to 8.
  const times = [];
  let at = performance.now();
  const ahead = () => {
    if (!times.length) return 3;
    const avg = times.reduce((a, b) => a + b) / times.length;
    return Math.min(8, Math.max(2, Math.ceil(2000 / avg)));
  };
  const prefetch = by => {
    const k = ahead();
    const keep = new Set([n, n - by]);
    for (let i = 1; i <= k; i++) keep.add(n + by * i);
    for (const m of keep) if (m >= 1 && m <= pages.p.length && m !== n) make(m, true);
    for (const m of made.keys()) if (!keep.has(m)) made.delete(m);
  };
  const stage = document.querySelector('.stage');
  const show = (m, by) => {
    const now = performance.now();
    if (now - at < 60000) { times.push(now - at); if (times.length > 5) times.shift(); }
    at = now;
    const e = make(m, false);
    e.im.fetchPriority = 'high';
    stage.replaceChildren(e.im);
    n = m;
    count.textContent = m + ' / ' + pages.p.length;
    document.title = document.title.replace(/· \d+\/\d+ ·/, '· ' + m + '/' + pages.p.length + ' ·');
    history.replaceState(null, '', '/r/' + book + '/' + m + location.hash);
    store.set('pos:' + book, m);
    links();
    prefetch(by);
  };
  // Forward or back, past either end to the series. Before the pages are
  // known, the page's own links.
  const turn = by => {
    body.classList.remove('hint');
    if (!pages) {
      const rel = document.querySelector(by > 0 ? 'link[rel=next]' : 'link[rel=prev]');
      location.href = rel ? rel.href : '/s/' + series;
      return;
    }
    const m = n + by;
    if (m < 1 || m > pages.p.length) { location.href = '/s/' + series; return; }
    show(m, by);
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
  // The book's pages; then the next ones, made ahead.
  fetch('/b/' + book).then(r => r.ok ? r.json() : null).then(j => {
    if (!j) return;
    pages = j;
    // The page on screen came with the HTML: keep it as made.
    const shown = document.getElementById('pg');
    made.set(n, { im: shown, ready: Promise.resolve() });
    prefetch(1);
  });
})();
