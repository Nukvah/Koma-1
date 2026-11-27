// Koma-1 EPUB reader: the text's size, width and spacing (for every book);
// the bars on a tap or m; the contents on c; the chapters on the arrow keys
// and on space at the end of one; and the position, where the book opens
// next time, on any device. Those are kept with the account: read from the
// page, sent back as they change. Loaded in <head>, so the text is drawn in
// its size from the start.
(() => {
  const kept = JSON.parse(document.documentElement.dataset.kept || '{}'), sent = {};
  const flush = () => { if (Object.keys(sent).length) navigator.sendBeacon('/kept', new URLSearchParams(sent)); for (const k in sent) delete sent[k]; };
  const store = {
    get: k => kept[k] ?? null,
    set: (k, v) => { kept[k] = sent[k] = '' + v; clearTimeout(store.t); store.t = setTimeout(flush, 1e3); },
  };
  addEventListener('pagehide', flush);
  const WIDTH = { narrow: '28em', medium: '34em', wide: '44em' };
  const SPACE = { tight: '1.4', normal: '1.65', loose: '1.9' };
  let size = Math.min(28, Math.max(14, +store.get('esize') || 19));
  let width = WIDTH[store.get('ewidth')] ? store.get('ewidth') : 'medium';
  let space = SPACE[store.get('espace')] ? store.get('espace') : 'normal';
  const look = () => {
    const s = document.documentElement.style;
    s.setProperty('--s', size + 'px');
    s.setProperty('--w', WIDTH[width]);
    s.setProperty('--l', SPACE[space]);
  };
  look();

  addEventListener('DOMContentLoaded', () => {
    const $ = q => document.querySelector(q);
    const $$ = q => document.querySelectorAll(q);
    const body = document.body;
    const book = body.dataset.book;
    const n = +body.dataset.n;
    const total = +body.dataset.total;

    // The position: the chapter, and the first block of text on screen.
    const blocks = [...$$('main :is(p,h1,h2,h3,h4,h5,h6,li,blockquote,pre,figure,img,tr,dt,dd,hr,aside)')];
    const at = () => Math.max(0, blocks.findIndex(b => b.getBoundingClientRect().bottom > 1));
    const keep = () => store.set('epos:' + book, n + ':' + at());
    // Opened from outside the book (the series, a link): where it was left.
    const [c, i] = (store.get('epos:' + book) || '').split(':').map(Number);
    if (!location.hash && c >= 1 && c <= total) {
      if (c !== n && !document.referrer.includes('/e/' + book + '/')) return location.replace(c);
      if (c === n && blocks[i]) blocks[i].scrollIntoView();
    }
    let t;
    addEventListener('scroll', () => { clearTimeout(t); t = setTimeout(keep, 250); }, { passive: true });
    addEventListener('pagehide', () => { keep(); flush(); });
    keep();

    // Size, width, spacing: the same block stays on screen.
    const marks = () => {
      for (const a of $$('[data-width]')) a.toggleAttribute('aria-current', a.dataset.width === width);
      for (const a of $$('[data-space]')) a.toggleAttribute('aria-current', a.dataset.space === space);
      $('#size').textContent = size;
    };
    const change = f => {
      const b = blocks[at()];
      f();
      look();
      marks();
      if (b) b.scrollIntoView();
      keep();
    };
    const on = (q, f) => { for (const a of $$(q)) a.addEventListener('click', e => { e.preventDefault(); change(() => f(a)); }); };
    on('[data-size]', a => { size = Math.min(28, Math.max(14, size + +a.dataset.size)); store.set('esize', size); });
    on('[data-width]', a => { width = a.dataset.width; store.set('ewidth', width); });
    on('[data-space]', a => { space = a.dataset.space; store.set('espace', space); });
    for (const e of $$('.ebar [hidden]')) e.hidden = false;
    marks();

    // The bars and the contents.
    const bars = open => body.classList.toggle('open', open ?? !body.classList.contains('open'));
    const toc = open => {
      if (body.classList.toggle('toc-open', open ?? !body.classList.contains('toc-open'))) {
        const cur = $('.toc [aria-current]');
        if (cur) cur.scrollIntoView({ block: 'center' });
      }
      if (location.hash === '#toc') history.replaceState(null, '', location.pathname);
    };
    for (const a of $$('a[href="#toc"]')) a.addEventListener('click', e => { e.preventDefault(); bars(false); toc(true); });
    $('.toc .shut').addEventListener('click', e => { e.preventDefault(); toc(false); });
    // A tap on the text (not a link, not a selection) shows or hides the bars.
    $('main').addEventListener('click', e => {
      if (e.target.closest('a') || String(getSelection())) return;
      toc(false);
      bars();
    });
    const go = rel => { const a = $('.chap a[rel=' + rel + ']'); if (a) location.href = a.href; };
    addEventListener('keydown', e => {
      if (e.altKey || e.ctrlKey || e.metaKey) return;
      const k = e.key;
      const end = innerHeight + scrollY >= document.documentElement.scrollHeight - 2;
      if (k === 'ArrowRight' || k === 'l') go('next');
      else if (k === 'ArrowLeft' || k === 'h') go('prev');
      else if ((k === ' ' && !e.shiftKey || k === 'PageDown') && end) go('next');
      else if (k === 'm' || k === 'Enter') bars();
      else if (k === 'c') toc();
      else if (k === 'Escape') { toc(false); bars(false); }
      else return;
      e.preventDefault();
    });
  });
})();
