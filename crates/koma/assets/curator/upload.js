// Koma-1: adding files (docs/design/upload.html). A drop is read by name
// only, the server plans it, and the review sheet shows the plan; nothing is
// sent before "add". Files go in pieces that resume after a dropped
// connection; photo folders are sent image by image and packed on the
// server. Then the server's worker files the drop as one change, with undo.
(() => {
  const root = document.querySelector('[data-add]');
  if (!root) return;
  const ds = root.dataset;
  const review = document.querySelector('[data-review]');
  const CHUNK = 8 << 20;
  const META = /(^|\/)(metadata\.json|comicinfo\.xml)$/i;
  let files = [], items = [], plan = null, stop = false;

  const el = (tag, attrs, ...kids) => {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k === 'class') e.className = v; else if (k in e) e[k] = v; else e.setAttribute(k, v);
    }
    e.append(...kids.filter(k => k != null));
    return e;
  };
  const size = n => n < 1 << 20 ? Math.max(1, Math.round(n / 1024)) + ' KB'
    : n < 1 << 30 ? (n / 1048576).toFixed(0) + ' MB' : (n / 1073741824).toFixed(1) + ' GB';
  const sleep = ms => new Promise(ok => setTimeout(ok, ms));
  const api = async (method, url, body) => {
    const r = await fetch(url, { method, body, headers: body && typeof body === 'string' ? { 'Content-Type': 'application/json' } : {} });
    let j = {};
    try { j = await r.json(); } catch (_) { /* no body */ }
    return { ok: r.ok, status: r.status, j };
  };
  const today = () => new Date().toISOString().slice(0, 10);
  const base = p => p.slice(p.lastIndexOf('/') + 1);
  const landing = j => (std.checked && j.to) || j.name.replace(/\.(cb7|7z)$/i, '.cbz');

  // ---- reading the drop

  const add = (f, path) => { files.push(f); items.push({ path, size: f.size }); };
  const walk = async (entry, path) => {
    if (entry.isFile) {
      add(await new Promise((ok, no) => entry.file(ok, no)), path + entry.name);
    } else if (entry.isDirectory) {
      const r = entry.createReader();
      for (;;) {
        const batch = await new Promise((ok, no) => r.readEntries(ok, no));
        if (!batch.length) break;
        for (const e of batch) await walk(e, path + entry.name + '/');
      }
    }
  };
  const start = () => { files = []; items = []; plan = null; review.textContent = ''; };
  root.addEventListener('dragover', e => { e.preventDefault(); root.classList.add('over'); });
  root.addEventListener('dragleave', () => root.classList.remove('over'));
  root.addEventListener('drop', async e => {
    e.preventDefault();
    root.classList.remove('over');
    start();
    const entries = [...e.dataTransfer.items].map(i => i.webkitGetAsEntry && i.webkitGetAsEntry()).filter(Boolean);
    if (entries.length) for (const en of entries) await walk(en, '');
    else for (const f of e.dataTransfer.files) add(f, f.name);
    ask();
  });
  for (const input of root.querySelectorAll('input[type=file]')) {
    input.addEventListener('change', () => {
      start();
      for (const f of input.files) add(f, f.webkitRelativePath || f.name);
      input.value = '';
      ask();
    });
  }

  // ---- the plan

  const std = el('input', { type: 'checkbox', checked: true });
  async function ask() {
    if (!items.length) return;
    const meta = {};
    for (let i = 0; i < items.length; i++) {
      if (META.test(items[i].path) && files[i].size < 1 << 20) meta[i] = await files[i].text();
    }
    const body = { library: ds.library, items, meta, today: today() };
    if (ds.series) body.series = +ds.series;
    if (ds.collection) body.collection = ds.collection;
    const r = await api('POST', '/up/plan', JSON.stringify(body));
    if (!r.ok) { review.append(el('p', { class: 'note err' }, 'The drop could not be planned: ' + (r.j.error || r.status))); return; }
    plan = r.j;
    show();
  }

  function show() {
    review.textContent = '';
    const jobs = plan.jobs;
    const fresh = new Set(jobs.filter(j => j.new).map(j => j.path)).size;
    review.append(el('div', { class: 'pair' },
      el('section', { class: 'box' }, el('h2', {}, 'what the drop holds'), el('div', { class: 'status' },
        el('p', {}, items.length + ' files · ' + size(items.reduce((s, i) => s + i.size, 0))),
        el('p', { class: 'hint' }, 'Read in your browser by name: no file has been sent yet.'))),
      el('section', { class: 'box' }, el('h2', {}, 'this plan'), el('div', { class: 'status' },
        el('p', {}, fresh + ' new series · ' + jobs.length + ' files · ' + plan.refused.length + ' not taken'),
        el('p', {}, 'Adding it is one journaled change, with undo: a book goes to the trash if you undo.'),
        el('p', {}, el('label', {}, std, ' names to the ', el('a', { href: '/naming?l=' + encodeURIComponent(plan.library) }, 'naming standard')))))));
    const body = el('tbody');
    let last = null;
    jobs.forEach(j => {
      if (j.path !== last) {
        last = j.path;
        body.append(el('tr', {}, el('th', { colSpan: 3, scope: 'rowgroup' }, (j.path || '~') + '/ · ', el('span', { class: 'hint' }, j.new ? 'new series' : 'already in the library'))));
      }
      const dir = j.kind === 'pack' ? items[j.pages[0]].path.slice(0, items[j.pages[0]].path.lastIndexOf('/') + 1) : '';
      const from = j.kind === 'pack' ? (dir || 'loose images ') + ' · ' + j.pages.length + ' images' : items[j.file].path;
      const to = el('div', { class: 'to' });
      const paint = () => { to.textContent = landing(j); };
      paint();
      std.addEventListener('change', paint);
      j.on = el('input', { type: 'checkbox', checked: true });
      j.row = el('tr', {}, el('td', {}, el('label', { class: 'hint' }, j.on, j.kind === 'pack' ? ' pack' : ' add')),
        el('td', {}, el('div', { class: 'from' }, from), to,
          j.note ? el('div', { class: 'hint' }, 'keeps its name: ' + j.note) : null,
          j.kind === 'pack' ? el('div', { class: 'hint' }, 'packed on the server as the images arrive') : null),
        el('td', {}, size(j.size)));
      body.append(j.row);
    });
    for (const r of plan.refused) {
      body.append(el('tr', { class: 'warn' }, el('td', {}, el('span', { class: 'pending' }, 'not taken')),
        el('td', {}, el('div', { class: 'from' }, items[r.file].path), el('div', { class: 'hint' }, r.why)), el('td', {}, size(items[r.file].size))));
    }
    const go = el('button', { class: 'btn primary', type: 'button' });
    const count = () => {
      const on = jobs.filter(j => j.on.checked);
      go.textContent = 'add ' + on.length + ' · ' + size(on.reduce((s, j) => s + j.size, 0));
      go.disabled = !on.length;
    };
    for (const j of jobs) j.on.addEventListener('change', count);
    count();
    go.addEventListener('click', send);
    const cancel = el('button', { class: 'btn', type: 'button' }, 'cancel');
    cancel.addEventListener('click', () => { stop = true; start(); });
    review.append(el('h2', { class: 'section-h' }, 'review · nothing is sent until you add it'),
      el('div', { class: 'scroll' }, el('table', { class: 'plan' }, body)), el('div', { class: 'actbar' }, go, cancel));
  }

  // ---- sending

  async function retry(what) {
    for (let t = 0; ; t++) {
      if (stop) throw new Error('cancelled');
      try {
        const r = await what();
        if (r.status < 500 && r.status !== 409) return r;
        if (r.status === 409 && r.j.received !== undefined) return r;
        if (r.status === 409 && r.j.error === 'already in this pack') return r;
      } catch (_) { /* the network: try again */ }
      if (t >= 8) throw new Error('the connection kept dropping');
      note.textContent = 'connection lost, trying again in ' + (1 << Math.min(t, 5)) + ' s';
      await sleep(1000 << Math.min(t, 5));
    }
  }

  let sent = 0, total = 0;
  const meter = el('i', {});
  const line = el('small', {});
  const note = el('span', { class: 'pending' });
  const tick = n => {
    sent += n;
    meter.style.setProperty('--p', Math.round(sent * 100 / Math.max(total, 1)) + '%');
  };

  async function sendFile(j) {
    const f = files[j.file];
    const r = await retry(() => api('POST', '/up', JSON.stringify({ library: plan.library, kind: 'file', name: j.name, size: f.size, batch })));
    if (!r.ok) throw new Error(r.j.error || 'refused');
    const id = r.j.id;
    let at = 0;
    while (at < f.size) {
      const end = Math.min(at + CHUNK, f.size);
      const p = await retry(() => api('PATCH', '/up/' + id + '?offset=' + at, f.slice(at, end)));
      if (p.j.received === undefined) throw new Error(p.j.error || 'refused');
      tick(p.j.received - at);
      at = p.j.received;
      note.textContent = '';
    }
    return id;
  }

  async function sendPack(j) {
    const r = await retry(() => api('POST', '/up', JSON.stringify({ library: plan.library, kind: 'pack', name: j.name, batch })));
    if (!r.ok) throw new Error(r.j.error || 'refused');
    const id = r.j.id;
    for (const i of [...j.pages, ...j.extras]) {
      const p = await retry(() => api('PUT', '/up/' + id + '/m?name=' + encodeURIComponent(base(items[i].path)), files[i]));
      if (!p.ok && p.status !== 409) throw new Error(base(items[i].path) + ': ' + (p.j.error || 'refused'));
      tick(files[i].size);
      note.textContent = '';
    }
    return id;
  }

  let batch = '';
  async function send() {
    stop = false;
    batch = Math.random().toString(36).slice(2, 12);
    const jobs = plan.jobs.filter(j => j.on.checked);
    total = jobs.reduce((s, j) => s + j.size, 0);
    sent = 0;
    const cancel = el('button', { class: 'btn danger', type: 'button' }, 'cancel');
    cancel.addEventListener('click', () => { stop = true; });
    const box = el('section', { class: 'job' }, el('div', {}, el('b', {}, 'Adding ' + jobs.length + ' files to ~/' + plan.library), el('br'), line),
      el('div', { class: 'row' }, cancel), el('div', { class: 'meter', role: 'progressbar' }, meter), el('small', {}, note));
    review.textContent = '';
    review.append(el('h2', { class: 'section-h' }, 'sending · it carries on after a dropped connection'), box);
    const done = [], failed = [];
    let cancelled = 0;
    for (const [k, j] of jobs.entries()) {
      line.textContent = (k + 1) + ' of ' + jobs.length + ' · ' + size(sent) + ' of ' + size(total) + ' · ' + j.name;
      if (stop) { cancelled++; continue; }
      try {
        const id = j.kind === 'pack' ? await sendPack(j) : await sendFile(j);
        const d = await retry(() => api('POST', '/up/' + id + '/done'));
        if (!d.ok) throw new Error(d.j.error || 'refused');
        done.push({ upload: id, path: j.path, name: landing(j) });
      } catch (e) {
        if (stop) cancelled++; else failed.push({ name: j.name, why: e.message });
      }
    }
    for (const r of plan.refused) failed.push({ name: items[r.file].path, why: r.why, refused: true });
    line.textContent = 'filing…';
    const c = await api('POST', '/up/commit', JSON.stringify({ library: plan.library, batch, jobs: done, failed, cancelled, ignored: plan.ignored.length }));
    review.textContent = '';
    const text = c.ok ? (c.j.message || 'Sent: the worker is filing it') : 'The drop could not be handed over: ' + (c.j.error || c.status);
    review.append(el('div', { class: 'toast', role: 'status' }, el('span', {}, el('b', {}, text), ' · ', el('a', { href: '/jobs' }, 'jobs'), ' · ', el('a', { href: '/uploads' }, 'upload log'))));
  }
})();
