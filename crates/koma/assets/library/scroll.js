// Koma-1 library pages: infinite scroll with windowing. Without this script
// the ?page= links do the same, a page at a time.
(() => {
  const grid = document.querySelector('.grid[data-scroll]');
  if (!grid || !('IntersectionObserver' in window)) return;
  // The camera: a card far from view keeps only its box, at the height
  // every card has (measured once, never per card: no layout to wait for);
  // its contents wait here. The DOM and memory stay flat however far the
  // library goes.
  const away = new Map();
  let height = 0;
  const box = c => { c.style.height = height + 'px'; };
  addEventListener('resize', () => {
    height = 0;
    for (const c of away.keys()) c.style.height = '';
  });
  const cam = new IntersectionObserver(es => {
    for (const { target: c, isIntersecting: near } of es) {
      if (near) {
        if (away.has(c)) { c.innerHTML = away.get(c); away.delete(c); c.style.height = ''; }
      } else if (!away.has(c)) {
        height ||= grid.firstElementChild.offsetHeight;
        away.set(c, c.innerHTML);
        c.textContent = '';
        box(c);
      }
    }
  }, { rootMargin: '300% 0px' });
  // The URL follows the chunk at the top of the view once scrolling stops,
  // so reload and back return there.
  const marks = [];
  let settle;
  addEventListener('scroll', () => {
    clearTimeout(settle);
    settle = setTimeout(() => {
      let at = marks[0];
      for (const m of marks) if (m.getBoundingClientRect().top <= 1) at = m;
      if (at && location.pathname + location.search !== at.dataset.url) {
        history.replaceState(null, '', at.dataset.url);
      }
    }, 150);
  }, { passive: true });
  const add = (cards, url) => cards.forEach((c, i) => {
    cam.observe(c);
    if (!i) { c.dataset.url = url; marks.push(c); }
  });
  add([...grid.children], location.pathname + location.search);
  let pager = document.querySelector('nav.pager');
  let busy = false;
  // Near the end, the next chunk: the next ?page=, its cards appended.
  const more = new IntersectionObserver(async es => {
    if (busy || !es.some(e => e.isIntersecting)) return;
    const next = pager.querySelector('a[rel=next]');
    if (!next) return more.disconnect();
    busy = true;
    try {
      const r = await fetch(next.href);
      if (!r.ok) return;
      const doc = new DOMParser().parseFromString(await r.text(), 'text/html');
      const cards = [...doc.querySelector('.grid').children];
      grid.append(...cards);
      const u = new URL(next.href);
      add(cards, u.pathname + u.search);
      const np = doc.querySelector('nav.pager');
      more.disconnect();
      if (!np) { pager.remove(); return; }
      pager.replaceWith(np);
      pager = np;
      more.observe(pager);
    } finally {
      busy = false;
    }
  }, { rootMargin: '200% 0px' });
  if (pager) more.observe(pager);
})();
