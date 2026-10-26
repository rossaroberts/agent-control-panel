// Markdown for the Context page, made safe to show. The page reads files agents wrote (memory notes written from emails
// and invoices among them), and script running on the page could take the panel's token, which starts sessions with every
// permission allowed. So marked's output is never trusted as it comes:
//   · raw HTML in a document is shown as text, never rendered (block: in a code block; inline: as plain text);
//   · a link is an <a> only for http(s) and, in a document that has them, a note of the memory folder; any other
//     (javascript:, data:, mailto:, a relative path…) is its words, as text;
//   · an image is its alt text, never loaded.
// This file runs in the browser (an ES module) and under Node, in the tests. `marked` is passed in: the vendored UMD
// build is a global in the page, and is loaded from public/vendor/marked.umd.js in the tests.

export const esc = (s) => String(s ?? '').replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');

/** A note of the memory folder: a plain `name.md`, never a path. Returns the name, or null. */
export function noteName(href) {
  if (typeof href !== 'string' || href.length > 200) return null;
  let h = href;
  try { h = decodeURIComponent(href); } catch { return null; }
  return /^[A-Za-z0-9_][\w.-]{0,120}\.md$/.test(h) && !h.includes('..') ? h : null;
}

/** What a link may be: { web: normalised url } for http(s), { note: name } for a memory note when `notes` is on, else null. */
export function linkOf(href, notes) {
  if (typeof href !== 'string') return null;
  const n = notes ? noteName(href) : null;
  if (n) return { note: n };
  try {
    const u = new URL(href.trim());
    return u.protocol === 'http:' || u.protocol === 'https:' ? { web: u.href } : null;
  } catch { return null; }
}

/** Markdown to HTML. `notes` is the id of the document whose relative `name.md` links open in the tab (the memory index and
 *  its notes), or empty. A fresh marked instance each time: nothing leaks between documents. */
export function renderMarkdown(marked, md, { notes = '' } = {}) {
  const m = new marked.Marked({ gfm: true, breaks: false, async: false, renderer: {
    html(token) {
      const text = esc(token.text ?? token.raw ?? '');
      return token.block ? `<pre class="raw"><code>${text.replace(/\n+$/, '')}</code></pre>\n` : text;
    },
    link(token) {
      const inner = this.parser.parseInline(token.tokens ?? []);
      const l = linkOf(token.href, notes);
      if (!l) return inner;
      const title = token.title ? ` title="${esc(token.title)}"` : '';
      return l.note ? `<a href="#${esc(notes)}/${esc(encodeURIComponent(l.note))}" data-note="${esc(l.note)}"${title}>${inner}</a>`
        : `<a href="${esc(l.web)}" target="_blank" rel="noopener noreferrer"${title}>${inner}</a>`;
    },
    image(token) { return `<span class="noimg">[image${token.text ? `: ${esc(token.text)}` : ''}]</span>`; },
  } });
  return m.parse(String(md ?? ''));
}

/** The html a Context document is shown as, or a plain-text fallback if marked itself fails: never an error on the page. */
export function renderSafe(marked, md, opts) {
  try { return renderMarkdown(marked, md, opts); } catch { return `<pre class="raw"><code>${esc(md)}</code></pre>`; }
}
