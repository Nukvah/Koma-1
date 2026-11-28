// Koma-1 curator pages: the jobs page follows the jobs live (one event
// stream), and in select mode shift-click picks a run of books. The pages
// work without it.
(() => {
  // Jobs: progress and state as the worker reports them; a job that ends
  // (or one not on the page yet) reloads the page.
  if (document.querySelector('[data-jobs]') && window.EventSource) {
    const es = new EventSource('/jobs/events');
    es.onmessage = e => {
      const j = JSON.parse(e.data);
      const box = document.querySelector('[data-op="' + j.id + '"]');
      if (!box || (j.state !== 'running' && j.state !== 'paused')) {
        if (!box || box.dataset.state !== j.state) { es.close(); location.reload(); }
        return;
      }
      box.dataset.state = j.state;
      const t = box.querySelector('[data-text]');
      if (t) t.textContent = (j.state === 'paused' ? 'paused at ' : '') + j.done + ' of ' + j.total;
      const m = box.querySelector('.meter i');
      if (m && j.total) m.style.setProperty('--p', Math.round(j.done * 100 / j.total) + '%');
    };
  }

  // Select mode: shift-click picks every book between two.
  let last = null;
  const boxes = [...document.querySelectorAll('.pick input[type=checkbox]')];
  for (const b of boxes) {
    b.addEventListener('click', e => {
      if (e.shiftKey && last) {
        const [a, z] = [boxes.indexOf(last), boxes.indexOf(b)].sort((x, y) => x - y);
        for (const x of boxes.slice(a, z + 1)) x.checked = b.checked;
      }
      last = b;
    });
  }
})();
