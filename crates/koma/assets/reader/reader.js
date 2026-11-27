// Koma-1 reader: turns pages in place (keys, tap thirds, swipes) in the
// book's direction: one page, two, or a vertical strip; fitted to the
// window, its width or height; zoomed. The next pages are decoded ahead as
// far as the reading speed needs. Bars with a scrubber and thumbnails; the
// next book at the end. Position and the series' settings stay in this
// browser (accounts, M5, will carry them). Without it, the page's links do
// the same a page load at a time.
(() => {
  const $ = q => document.querySelector(q);
  const $$ = q => document.querySelectorAll(q);
  const on = (t, e, f, o) => t.addEventListener(e, f, o);
  const body = document.body;
  const book = body.dataset.book;
  const series = body.dataset.series;
  const stage = $('.stage');
  const zones = $$('.zones a');
  const count = $('#count');
  const store = {
    get: k => { try { return localStorage.getItem('koma:' + k); } catch (e) { return null; } },
    set: (k, v) => { try { localStorage.setItem('koma:' + k, v); } catch (e) {} },
  };
  const mine = k => store.get(k + ':' + series);
  const keep = (k, v) => (store.set(k + ':' + series, v), v);
  let dir = mine('dir') || body.dataset.dir;
  let mode = mine('mode') || 'auto';
  let shift = mine('shift') === '1';
  let fit = mine('fit') || 'page';
  const rtl = () => dir === 'rtl';
  const flip = () => { shift = keep('shift', shift ? 0 : 1) === 1; };
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
  // The smallest picture of a page, for thumbnails.
  const small = p => {
    const v = p.s.find(([, m]) => m & (webp ? 2 : 1));
    return v ? url(p, v[0], webp ? 'webp' : 'jpg') : url(p, 0, p.f);
  };
  // Page m's picture, its size known before it loads; no source yet.
  const image = (m, sizes) => {
    const p = P(m);
    const im = new Image();
    im.sizes = sizes;
    im.width = p.w; im.height = p.ht;
    im.alt = 'Page ' + m;
    im.dataset.n = m;
    return im;
  };
  const wide = m => P(m).w > P(m).ht * 1.2;
  const tall = m => P(m).ht >= P(m).w * 3;

  // Pages made ahead: each decoded before it is needed, so a turn only
  // swaps it in. Single and double pages; the strip loads as it scrolls.
  const made = new Map();
  const sizes = (m, two) => {
    const w = (two ? 50 : 100) + 'vw', h = (100 * P(m).w / P(m).ht).toFixed(1) + 'vh';
    return fit === 'width' ? w : fit === 'height' ? h : 'min(' + w + ', ' + h + ')';
  };
  const key = m => m + (view === 'double' ? 'd' : 's');
  const make = m => {
    let e = made.get(key(m));
    if (!e) {
      const im = image(m, sizes(m, view === 'double'));
      im.fetchPriority = 'low';
      im.srcset = srcset(P(m));
      e = { im, ready: im.decode().catch(() => {}) };
      made.set(key(m), e);
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
      keep.add(key(m));
      make(m);
    }
    for (const key of made.keys()) if (!keep.has(key)) made.delete(key);
  };

  const shown = m => {
    n = m;
    count.textContent = m + ' / ' + N();
    document.title = document.title.replace(/· \d+\/\d+ ·/, '· ' + m + '/' + N() + ' ·');
    history.replaceState(null, '', '/r/' + book + '/' + m + location.hash);
    store.set('pos:' + book, m);
    scrub.value = m;
    mark();
  };
  // A page larger than the window (fit width or height, zoom) is read from
  // its start: the top; the left, or the right in right-to-left books.
  // Going back, from its end.
  const doc = document.scrollingElement;
  const start = by => {
    const end = by < 0;
    scrollTo(fit === 'height' && rtl() !== end ? doc.scrollWidth : 0, fit === 'width' && end ? doc.scrollHeight : 0);
  };
  const room = by => {
    if (fit === 'width') return by > 0 ? doc.scrollTop + innerHeight < doc.scrollHeight - 2 : doc.scrollTop > 0;
    if (fit !== 'height') return false;
    const left = (by > 0) === rtl();
    return left ? doc.scrollLeft > 0 : doc.scrollLeft + innerWidth < doc.scrollWidth - 2;
  };
  const show = (m, by) => {
    const now = performance.now();
    if (now - at < 60000) { times.push(now - at); if (times.length > 5) times.shift(); }
    at = now;
    const list = view === 'double' ? groups[group(m)] : [m];
    const ims = list.map(i => { const e = make(i); e.im.fetchPriority = 'high'; return e.im; });
    zoom(false);
    stage.replaceChildren(...ims);
    shown(list[0]);
    if (fit !== 'page') start(by);
    prefetch(by);
  };
  // Forward or back, past either end to the series. Before the pages are
  // known, the page's own links.
  const turn = by => {
    body.classList.remove('hint');
    // Pinched in (phones): the finger pans, it does not turn.
    if (window.visualViewport && visualViewport.scale > 1.05) return;
    if (card) { if (by > 0) location.href = card.querySelector('a').href; else close(); return; }
    if (zoomed) zoom(false);
    if (!pages) {
      const rel = $(by > 0 ? 'link[rel=next]' : 'link[rel=prev]');
      location.href = rel ? rel.href : '/s/' + series;
      return;
    }
    if (view === 'strip') {
      if (by > 0 && doc.scrollTop + innerHeight >= doc.scrollHeight - 2) return ending();
      return scrollBy(0, by * innerHeight * 0.9);
    }
    if (room(by)) return fit === 'width' ? scrollBy(0, by * innerHeight * 0.9) : scrollBy((by > 0) === rtl() ? -innerWidth * 0.9 : innerWidth * 0.9, 0);
    let m;
    if (view === 'double') {
      const g = groups[group(n) + by];
      m = g && g[0];
    } else m = n + by;
    if (!m || m < 1 || m > N()) { if (by > 0) ending(); else location.href = '/s/' + series; return; }
    show(m, by);
  };
  const go = m => {
    if (view === 'strip') stage.children[m - 1].scrollIntoView();
    else show(m, m < n ? -1 : 1);
  };

  // Zoom (z, or the bars): twice the size, sharper pictures, panned by
  // scrolling or dragging; out again on z, Escape, a double click or a turn.
  let zoomed = false;
  const zoom = on => {
    if (on === zoomed || (on && view === 'strip')) return;
    zoomed = on;
    const ims = [...stage.querySelectorAll('img')];
    const ws = ims.map(im => im.clientWidth * 2);
    body.classList.toggle('zoom', on);
    ims.forEach((im, i) => {
      im.style.width = on ? ws[i] + 'px' : '';
      im.sizes = on ? ws[i] + 'px' : sizes(+im.dataset.n, view === 'double');
    });
    if (on) scrollTo((doc.scrollWidth - innerWidth) / 2, (doc.scrollHeight - innerHeight) / 2);
    else start(1);
  };
  on(stage, 'pointermove', e => { if (zoomed && e.buttons && e.pointerType === 'mouse') scrollBy(-e.movementX, -e.movementY); });
  on(stage, 'dragstart', e => e.preventDefault());
  on(stage, 'dblclick', () => zoom(false));

  // The scrubber: a preview of the page under it while it moves; the page
  // when it is let go.
  const scrub = $('#scrub');
  const peek = scrub.nextElementSibling;
  const peekIm = new Image();
  on(scrub, 'input', () => {
    const m = +scrub.value;
    peekIm.src = small(P(m));
    peek.replaceChildren(peekIm, 'p ' + m);
    peek.style.setProperty('--p', (100 * (m - 1) / Math.max(1, N() - 1)) + '%');
    scrub.parentNode.classList.add('on');
  });
  on(scrub, 'change', () => { scrub.parentNode.classList.remove('on'); go(+scrub.value); });

  // Thumbnails (t): every page, loaded as they scroll into the strip.
  const thumbs = $('.thumbs');
  on(thumbs, 'click', e => { const a = e.target.closest('a'); if (a) { e.preventDefault(); go(+a.dataset.n); } });
  const mark = () => {
    if (thumbs.hidden) return;
    for (const a of thumbs.children) a.toggleAttribute('aria-current', +a.dataset.n === n);
    const at = thumbs.children[n - 1];
    at && at.scrollIntoView({ block: 'nearest', inline: 'center' });
  };
  const thumbsOn = () => {
    if (!pages) return;
    thumbs.hidden = !thumbs.hidden;
    if (!thumbs.hidden) bars(true);
    if (!thumbs.childElementCount) {
      thumbs.innerHTML = pages.p.map((p, i) => '<a href="/r/' + book + '/' + ++i + '" data-n=' + i + '><img loading=lazy src="' + small(p) + '" width=' + p.w + ' height=' + p.ht + ' alt="Page ' + i + '"></a>').join('');
    }
    mark();
  };

  // The end of the book (made by the server): the next one in the series,
  // from where it was left, or back to the series.
  let card = null;
  const close = () => { card.remove(); card = null; };
  const ending = () => {
    bars(false);
    card = document.createElement('div');
    card.className = 'next';
    card.innerHTML = pages.end;
    const a = card.querySelector('[data-next]');
    const at = a && store.get('pos:' + a.dataset.next);
    if (at) { a.href = a.href.replace(/\d+$/, at); card.querySelector('.at').textContent = 'at page ' + at; }
    on(card, 'click', e => { if (e.target === card) close(); });
    body.append(card);
    card.querySelector('.btn').focus();
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
      const im = image(i + 1, 'min(100vw, 800px)');
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
    zoom(false);
    for (const f of ['width', 'height']) body.classList.toggle('fit-' + f[0], fit === f && view !== 'strip');
    if (strip && view !== 'strip') { strip.disconnect(); strip = null; scrollTo(0, 0); }
    made.clear();
    // The page the HTML came with stays on screen: no second fetch.
    const first = $('#pg');
    if (first && view === 'single' && fit === 'page') { first.removeAttribute('id'); first.dataset.n = n; made.set(n + 's', { im: first, ready: Promise.resolve() }); }
    if (view === 'strip') stripOn();
    else { pair(); show(n, 1); }
    for (const e of [scrub.parentNode, $('[data-zoom]'), $('[data-thumbs]')]) e.hidden = false;
    scrub.max = N();
    links();
  };

  // Tap thirds follow the direction: in right-to-left books the left third
  // goes forward.
  const links = () => {
    const t = rtl() ? ['‹ next', 'bars', 'previous ›'] : ['‹ previous', 'bars', 'next ›'];
    zones.forEach((a, i) => { a.firstChild.textContent = t[i]; });
    stage.classList.toggle('rtl', rtl());
    thumbs.classList.toggle('rtl', rtl());
    scrub.parentNode.dir = dir;
    for (const [k, v] of [['dir', dir], ['mode', mode], ['fit', fit], ['shift', shift && '']]) {
      for (const a of $$('a[data-' + k + ']')) a.toggleAttribute('aria-current', a.dataset[k] === v);
    }
  };
  const bars = open => {
    body.classList.toggle('open', open ?? !body.classList.contains('open'));
    if (location.hash) history.replaceState(null, '', location.pathname);
  };

  zones.forEach((a, i) => on(a, 'click', e => {
    e.preventDefault();
    body.classList.remove('hint');
    if (i === 1) return bars();
    turn((i === 0) === rtl() ? 1 : -1);
  }));
  // In the strip a tap anywhere shows or hides the bars.
  on(stage, 'click', () => { if (view === 'strip') bars(); });
  const setting = (sel, f) => {
    for (const a of $$(sel)) {
      on(a, 'click', e => { e.preventDefault(); f(a); if (pages) apply(); else links(); });
    }
  };
  setting('a[data-dir]', a => { dir = keep('dir', a.dataset.dir); });
  setting('a[data-mode]', a => { mode = keep('mode', a.dataset.mode); });
  setting('a[data-fit]', a => { fit = keep('fit', a.dataset.fit); });
  const act = (sel, f) => on($(sel), 'click', e => { e.preventDefault(); f(); });
  act('a[data-zoom]', () => { bars(false); zoom(!zoomed); });
  act('a[data-thumbs]', thumbsOn);
  setting('a[data-shift]', flip);
  on(window, 'keydown', e => {
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const k = e.key;
    const left = k === 'ArrowLeft' || k === 'h';
    const right = k === 'ArrowRight' || k === 'l';
    if (view === 'strip' && (k === 'ArrowDown' || k === 'ArrowUp')) return;
    if (left || right) turn(left === rtl() ? 1 : -1);
    else if (k === ' ' || k === 'j' || k === 'PageDown') turn(e.shiftKey ? -1 : 1);
    else if (k === 'k' || k === 'PageUp') turn(-1);
    else if (k === 'Enter' || k === 'm') bars();
    else if (k === 'Escape') { if (card) close(); else if (zoomed) zoom(false); else bars(false); }
    else if (k === 'z') zoom(!zoomed);
    else if (k === 't') thumbsOn();
    else if (k === 's' && view === 'double') { flip(); apply(); }
    else return;
    e.preventDefault();
  });
  // A swipe turns like the tap on the side it starts from (not where it
  // scrolls: the strip, fit height, zoom).
  let x0 = null;
  on(window, 'touchstart', e => { x0 = e.touches.length === 1 ? e.touches[0].clientX : null; }, { passive: true });
  on(window, 'touchend', e => {
    if (x0 === null || view === 'strip' || zoomed || fit === 'height') return;
    const dx = e.changedTouches[0].clientX - x0;
    x0 = null;
    if (Math.abs(dx) > 50) turn((dx > 0) === rtl() ? 1 : -1);
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
