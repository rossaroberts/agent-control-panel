// "Pair a phone" on the Fleet page: a button in the top bar that asks the panel for the pairing address and shows it as a QR
// to scan with the phone's camera. Kept out of fleet.html so the page itself stays one thing; it uses only what that page
// already has (its token, its colours). The QR encoder (vendor/qrcode.js) is fetched on the first press, not on every load.
(() => {
  const nav = document.querySelector('nav.top');
  if (!nav || typeof TOKEN === 'undefined' || matchMedia('(max-width: 720px)').matches) return; // a phone is what gets paired, not what pairs

  const css = document.createElement('style');
  css.textContent = `
    #pair-dlg { border: 1px solid var(--rule); background: var(--paper); color: var(--ink); font: inherit; max-width: 420px; padding: 24px 28px; border-radius: 0; }
    #pair-dlg::backdrop { background: color-mix(in srgb, var(--paper) 70%, transparent); }
    #pair-dlg h2 { font-size: 15px; font-weight: 500; margin: 0 0 12px; }
    #pair-dlg p { margin: 0 0 12px; font-size: 13px; color: var(--grey); }
    #pair-dlg .qr { background: #fff; padding: 12px; width: 264px; height: 264px; margin: 0 0 12px; }
    #pair-dlg .qr svg { display: block; width: 100%; height: 100%; }
    #pair-dlg .acts { display: flex; justify-content: flex-end; }
    #pair-dlg button { color: var(--blue); }
    #pair-dlg .err { color: var(--red); }`;
  document.head.append(css);

  const load = (src) => new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = src; s.onload = resolve; s.onerror = () => reject(new Error(`could not load ${src}`));
    document.head.append(s);
  });

  /** The QR as an SVG string: one path of one-module squares, black on a white tile (a scanner needs the contrast, in dark mode too). */
  function svgOf(text) {
    const qr = window.qrcode(0, 'M');
    qr.addData(text); qr.make();
    const n = qr.getModuleCount();
    let d = '';
    for (let r = 0; r < n; r++) for (let c = 0; c < n; c++) if (qr.isDark(r, c)) d += `M${c} ${r}h1v1h-1z`;
    return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${n} ${n}" shape-rendering="crispEdges" role="img" aria-label="Pairing code"><path d="${d}" fill="#000"/></svg>`;
  }

  async function show() {
    const dlg = document.createElement('dialog');
    dlg.id = 'pair-dlg';
    const h2 = document.createElement('h2'); h2.textContent = 'Pair a phone';
    const note = document.createElement('p'); note.textContent = 'Reading the address…';
    const acts = document.createElement('div'); acts.className = 'acts';
    const close = document.createElement('button'); close.textContent = 'Close'; close.onclick = () => dlg.close();
    acts.append(close);
    dlg.append(h2, note, acts);
    dlg.addEventListener('close', () => dlg.remove());
    document.body.append(dlg);
    dlg.showModal();
    try {
      const r = await fetch('/api/pair-url', { method: 'POST', headers: { 'content-type': 'application/json', 'x-acp-token': TOKEN }, body: '{}' }).then((x) => x.json());
      if (!r.ok) throw new Error(r.error || 'the panel did not answer');
      if (!r.public) {
        note.textContent = 'No public host is set, so a phone has nothing to reach. Put this Mac\'s Tailscale name in fleet.json as "public", run tailscale serve --bg 4747, restart the panel, and press this again.';
        return;
      }
      if (!window.qrcode) await load('/vendor/qrcode.js');
      const tile = document.createElement('div'); tile.className = 'qr'; tile.innerHTML = svgOf(r.url);
      // The QR only: the address is deliberately not printed (it would sit in screenshots and the clipboard). It is a one-time code, not the panel's key.
      const mins = Math.max(1, Math.round((r.expiresInMs || 120000) / 60000));
      note.textContent = 'Scan this with the phone\'s camera, once. It signs that phone in to this panel, so do not share the code.';
      const valid = document.createElement('p'); valid.textContent = `Valid for ${mins} minute${mins === 1 ? '' : 's'}, one use. Press Pair a phone again for another.`;
      dlg.insertBefore(tile, acts); dlg.insertBefore(valid, acts);
    } catch (e) {
      note.className = 'err'; note.textContent = String(e.message || e);
    }
  }

  const btn = document.createElement('button');
  btn.id = 'pair-phone'; btn.type = 'button'; btn.textContent = 'Pair a phone'; btn.title = 'Show a code that signs a phone in to this panel';
  btn.onclick = show;
  nav.insertBefore(btn, document.getElementById('refresh'));
})();
