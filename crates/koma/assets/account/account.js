// Koma-1 account pages: passkeys (sign in with one, add one), and what this
// browser kept before accounts, sent once with the sign-in form. Forms work
// without it; passkeys need it, and HTTPS (or localhost).
(() => {
  const $ = q => document.querySelector(q);
  const b64 = a => btoa(String.fromCharCode(...new Uint8Array(a))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const bin = s => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
  const post = (u, d) => fetch(u, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(d) })
    .then(r => r.ok ? r.json() : Promise.reject(r));
  const say = t => { const e = $('#pk-msg'); e.textContent = t; e.hidden = false; };
  const can = window.PublicKeyCredential && isSecureContext;
  const form = $('form[data-kept]');

  // The sign-in form: positions and settings this browser kept before
  // accounts. The server adds them where the account has none, then clears them.
  if (form) {
    try {
      const o = {};
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.startsWith('koma:')) o[k.slice(5)] = localStorage.getItem(k);
      }
      if (Object.keys(o).length) form.kept.value = JSON.stringify(o);
    } catch (e) {}
  }

  const signin = $('#pk-signin');
  if (signin && can) {
    $('#pk').hidden = false;
    signin.onclick = () => post('/passkey/signin/begin', {})
      .then(o => navigator.credentials.get({ publicKey: { challenge: bin(o.challenge), rpId: o.rpId, userVerification: 'preferred', timeout: 120000 } }))
      .then(c => post('/passkey/signin/finish', {
        id: b64(c.rawId), client: b64(c.response.clientDataJSON), auth: b64(c.response.authenticatorData),
        sig: b64(c.response.signature), next: form.next.value,
      }))
      .then(r => { location = r.next; })
      .catch(() => say('That passkey did not work. Try again, or use your password.'));
  }

  const add = $('#pk-add');
  if (add && can) {
    add.hidden = false;
    add.onclick = () => post('/passkey/new', {})
      .then(o => navigator.credentials.create({ publicKey: {
        challenge: bin(o.challenge), rp: { name: 'Koma-1', id: o.rpId },
        user: { id: bin(o.user), name: o.name, displayName: o.name },
        pubKeyCredParams: [{ type: 'public-key', alg: -7 }, { type: 'public-key', alg: -8 }],
        excludeCredentials: o.have.map(id => ({ type: 'public-key', id: bin(id) })),
        authenticatorSelection: { residentKey: 'required', userVerification: 'preferred' },
        attestation: 'none', timeout: 120000,
      } }))
      .then(c => post('/passkey/add', {
        client: b64(c.response.clientDataJSON), att: b64(c.response.attestationObject),
        name: (navigator.userAgentData && navigator.userAgentData.platform) || '',
      }))
      .then(() => location.reload())
      .catch(() => say('No passkey was added.'));
  }
})();
