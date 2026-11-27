// Koma-1 reader: turns pages in place (keys, tap thirds, swipes) in the
// book's reading direction, one page, two side by side, or one vertical
// strip; the next pages fetched and decoded ahead as far as the reading
// speed needs; the bars; and the position, the series' direction and mode
// kept in this browser (accounts, M5, will carry them across devices).
// Without it, the page's links do the same a page load at a time.
(() => {
  const body = document.body;
  const book = body.dataset.book;
  const series = body.dataset.series;
  const stage = document.querySelector('.stage');
  const zones = document.querySelectorAll('.zones a');
  const count = document.getElementById('count');
  const store = {
    get: k => { try { return localStorage.getItem('koma:' + k); } catch (e) { return null; } },
    set: (k, v) => { try { localStorage.setItem('koma:' + k, v); } catch (e) {} },
  };
  let dir = store.get('dir:' + series) || body.dataset.dir;
  let mode = store.get('mode:' + series) || 'auto';
  let shift = store.get('shift:' + series) === '1';
  let n = +body.dataset.n;
  let pages = null;
  let view = 'single';

  // A page's sources: its variants in one format (WebP where the browser
  // takes it, else JPEG) and itself. With Save-Data, the 720 one only.
  const webp = (() => {
    try { return document.createElement('canvas').toDataURL('image/webp').startsWith('data:image/webp'); } catch (e) { return false; }
  })();
  const save = !!(navigator.connection && navigator.connection.saveData);
  const P = m => pages.p[m - 1];
  const N = () => pages.p.length;
  const url = (p, w, f) => '/p/' + p.h + (w ? '.' + w + '.v' + pages.v + '.' : '.') + f;
  const srcset = p => {
    const f = webp ? 2 : 1;
    const vs = p.s.filter(([w, m]) => m & f && (!save || w === 720));
    const set = vs.map(([w]) => url(p, w, webp ? 'webp' : 'jpg') + ' ' + w + 'w');
    if (!save || !vs.length) set.push(url(p, 0, p.f) + ' ' + p.w + 'w');
    return set.join(', ');
  };
  const image = (m, sizes, low) => {
    const p = P(m);
    const im = new Image();
    im.fetchPriority = low ? 'low' : 'high';
    im.sizes = sizes;
    im.srcset = srcset(p);
    im.width = p.w; im.height = p.ht;
    im.alt = 'Page ' + m;
    return im;
  };
  const wide = m => P(m).w > P(m).ht * 1.2;
  const tall = m => P(m).ht >= P(m).w * 3;

  // Pages made ahead: each decoded before it is needed, so a turn only
  // swaps it in. Single and double pages; the strip loads as it scrolls.
  const made = new Map();
  const sizes = (m, two) => 'min(' + (two ? 50 : 100) + 'vw, ' + (100 * P(m).w / P(m).ht).toFixed(1) + 'vh)';
  const make = (m, low) => {
    const two = view === 'double';
    const key = m + (two ? 'd' : 's');
    let e = made.get(key);
    if (!e) {
      const im = image(m, sizes(m, two), low);
      e = { im, ready: im.decode().catch(() => {}) };
      made.set(key, e);
    }
    return e;
  };

  // Double pages: the cover alone, then pairs; a wide page (a spread
  // already) alone, the pairs starting again after it. The shift moves
  // every pair by one page.
  let groups = [];
  const pair = () => {
    groups = [];
    let i = 1;
    if (!shift) { groups.push([1]); i = 2; }
    while (i <= N()) {
      if (i === N() || wide(i) || wide(i + 1)) { groups.push([i]); i++; }
      else { groups.push([i, i + 1]); i += 2; }
    }
  };
  const group = m => groups.findIndex(g => g.includes(m));

  // Reading speed: the last few times between turns (pauses over a minute
  // left out). Ahead: enough for two seconds, 2 to 8 pages.
  const times = [];
  let at = performance.now();
  const ahead = () => {
    if (!times.length) return 3;
    const avg = times.reduce((a, b) => a + b) / times.length;
    return Math.min(8, Math.max(2, Math.ceil(2000 / avg)));
  };
  const prefetch = by => {
    const k = ahead();
    let want = [];
    if (view === 'double') {
      const g = group(n);
      for (let i = -1; i <= k; i++) want.push(...(groups[g + by * i] || []));
    } else {
      want = [n - by];
      for (let i = 0; i <= k; i++) want.push(n + by * i);
    }
    const keep = new Set();
    for (const m of want) {
      if (m < 1 || m > N()) continue;
      keep.add(m + (view === 'double' ? 'd' : 's'));
      make(m, true);
    }
    for (const key of made.keys()) if (!keep.has(key)) made.delete(key);
  };

  const shown = m => {
    n = m;
    count.textContent = m + ' / ' + N();
    document.title = document.title.replace(/· \d+\/\d+ ·/, '· ' + m + '/' + N() + ' ·');
    history.replaceState(null, '', '/r/' + book + '/' + m + location.hash);
    store.set('pos:' + book, m);
  };
  const show = (m, by) => {
    const now = performance.now();
    if (now - at < 60000) { times.push(now - at); if (times.length > 5) times.shift(); }
    at = now;
    const list = view === 'double' ? groups[group(m)] : [m];
    const ims = list.map(i => { const e = make(i, false); e.im.fetchPriority = 'high'; return e.im; });
    stage.replaceChildren(...ims);
    shown(list[0]);
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
    if (view === 'strip') return scrollBy(0, by * innerHeight * 0.9);
    let m;
    if (view === 'double') {
      const g = groups[group(n) + by];
      m = g && g[0];
    } else m = n + by;
    if (!m || m < 1 || m > N()) { location.href = '/s/' + series; return; }
    show(m, by);
  };

  // The strip: every page in one column; only those near the view hold
  // their picture, and the URL follows the page at the top.
  let strip = null;
  const stripOn = () => {
    strip && strip.disconnect();
    const cam = new IntersectionObserver(es => {
      for (const { target: im, isIntersecting: near } of es) {
        if (near) { if (!im.srcset) im.srcset = srcset(P(+im.dataset.n)); }
        else if (im.srcset) im.removeAttribute('srcset');
      }
    }, { rootMargin: '200% 0px' });
    const top = new IntersectionObserver(es => {
      for (const e of es) if (e.isIntersecting) shown(+e.target.dataset.n);
    }, { rootMargin: '0px 0px -95% 0px' });
    const ims = pages.p.map((p, i) => {
      const im = new Image();
      im.width = p.w; im.height = p.ht;
      im.alt = 'Page ' + (i + 1);
      im.sizes = 'min(100vw, 800px)';
      im.dataset.n = i + 1;
      cam.observe(im);
      top.observe(im);
      return im;
    });
    stage.replaceChildren(...ims);
    ims[n - 1].scrollIntoView();
    strip = { disconnect: () => { cam.disconnect(); top.disconnect(); } };
  };

  // The mode in use: as chosen for the series, or by itself: a strip when
  // most pages are tall strips, else one page.
  const apply = () => {
    const tallMost = pages.p.filter((_, i) => tall(i + 1)).length > N() / 2;
    view = mode === 'auto' ? (tallMost ? 'strip' : 'single') : mode;
    body.classList.toggle('strip', view === 'strip');
    stage.classList.toggle('two', view === 'double');
    if (strip && view !== 'strip') { strip.disconnect(); strip = null; scrollTo(0, 0); }
    made.clear();
    // The page the HTML came with stays on screen: no second fetch.
    const first = document.getElementById('pg');
    if (first && view === 'single') { first.removeAttribute('id'); made.set(n + 's', { im: first, ready: Promise.resolve() }); }
    if (view === 'strip') stripOn();
    else { pair(); show(n, 1); }
    links();
  };

  // Tap thirds follow the direction: in right-to-left books the left third
  // goes forward.
  const links = () => {
    const fwd = dir === 'rtl' ? 0 : 2;
    zones[fwd].querySelector('span').textContent = (dir === 'rtl' ? '‹ ' : '') + 'next' + (dir === 'rtl' ? '' : ' ›');
    zones[2 - fwd].querySelector('span').textContent = (dir === 'rtl' ? '' : '‹ ') + 'previous' + (dir === 'rtl' ? ' ›' : '');
    stage.classList.toggle('rtl', dir === 'rtl');
    const on = (sel, v) => {
      for (const a of document.querySelectorAll(sel)) {
        if (a.dataset[sel.slice(7, -1)] === v) a.setAttribute('aria-current', 'true'); else a.removeAttribute('aria-current');
      }
    };
    on('a[data-dir]', dir);
    on('a[data-mode]', mode);
    for (const a of document.querySelectorAll('a[data-shift]')) a.toggleAttribute('aria-current', shift);
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
  // In the strip a tap anywhere shows or hides the bars.
  stage.addEventListener('click', () => { if (view === 'strip') bars(); });
  const setting = (sel, f) => {
    for (const a of document.querySelectorAll(sel)) {
      a.addEventListener('click', e => { e.preventDefault(); f(a); if (pages) apply(); else links(); });
    }
  };
  setting('a[data-dir]', a => { dir = a.dataset.dir; store.set('dir:' + series, dir); });
  setting('a[data-mode]', a => { mode = a.dataset.mode; store.set('mode:' + series, mode); });
  setting('a[data-shift]', () => { shift = !shift; store.set('shift:' + series, shift ? '1' : '0'); });
  addEventListener('keydown', e => {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const k = e.key;
    const left = k === 'ArrowLeft' || k === 'h';
    const right = k === 'ArrowRight' || k === 'l';
    if (view === 'strip' && (k === 'ArrowDown' || k === 'ArrowUp')) return;
    if (left || right) turn(left === (dir === 'rtl') ? 1 : -1);
    else if (k === ' ' || k === 'j' || k === 'PageDown') turn(e.shiftKey ? -1 : 1);
    else if (k === 'k' || k === 'PageUp') turn(-1);
    else if (k === 'Enter' || k === 'm') bars();
    else if (k === 'Escape') bars(false);
    else if (k === 's' && view === 'double') { shift = !shift; store.set('shift:' + series, shift ? '1' : '0'); apply(); }
    else return;
    e.preventDefault();
  });
  // A swipe turns like the tap on the side it starts from (not in the
  // strip, which scrolls).
  let x0 = null;
  addEventListener('touchstart', e => { x0 = e.touches.length === 1 ? e.touches[0].clientX : null; }, { passive: true });
  addEventListener('touchend', e => {
    if (x0 === null || view === 'strip') return;
    const dx = e.changedTouches[0].clientX - x0;
    x0 = null;
    if (Math.abs(dx) > 50) turn((dx > 0) === (dir === 'rtl') ? 1 : -1);
  });

  // The tap zones, shown faintly the first time a reader opens.
  if (!store.get('seen')) { body.classList.add('hint'); store.set('seen', 1); }
  links();
  // The book's pages; then the mode, and the next pages made ahead.
  fetch('/b/' + book).then(r => r.ok ? r.json() : null).then(j => {
    if (!j) return;
    pages = j;
    apply();
  });
})();
