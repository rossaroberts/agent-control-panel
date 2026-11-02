// The requests the panel is holding, laid over Fleet's rows. Pure: the snapshot goes in, a copy comes out (the snapshot
// is Fleet's cache, shared by every page, so it is never written to). Held requests live on the Mac's panel, in server.mjs;
// fleet.mjs stays free of them.

/** The longest one string of a held request's input a row carries; what is cut says so, in the text (a Write's whole file would
 *  ride every poll, and a card that hid the tail of a command would be asking for a yes to something it did not show). */
const CLIP = 4000;
/** The most JSON a row carries of one request's input. Past it the input is replaced by `{ _truncated, bytes }`: the card says
 *  to answer on the Mac. (Cut strings are 4 kB each, so eight of them are over it.) */
const CAP = 16_384;
/** Questions and plans are read whole, up to this; a plan is something you read before you approve it. */
const WHOLE = new Set(['AskUserQuestion', 'ExitPlanMode']);
const WHOLE_CAP = 65_536;
const DEPTH = 8;

const clipText = (s) => (s.length > CLIP ? `${s.slice(0, CLIP)}\n… ${s.length - CLIP} more chars` : s);
/** `v` with every string in it cut to CLIP (at any depth); `hit.n` counts the cuts. */
function clipDeep(v, hit, depth = 0) {
  if (typeof v === 'string') { if (v.length > CLIP) hit.n++; return clipText(v); }
  if (!v || typeof v !== 'object') return v;
  if (depth >= DEPTH) { hit.n++; return '…'; }
  if (Array.isArray(v)) return v.map((x) => clipDeep(x, hit, depth + 1));
  return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, clipDeep(x, hit, depth + 1)]));
}
/** The input a row carries, and which of its top-level fields were cut. */
function shownInput(p) {
  const input = p.toolInput && typeof p.toolInput === 'object' && !Array.isArray(p.toolInput) ? p.toolInput : {};
  const whole = WHOLE.has(p.toolName);
  const sizeOf = () => { try { return Buffer.byteLength(JSON.stringify(input)); } catch { return 0; } };
  if (whole) return JSON.stringify(input).length > WHOLE_CAP ? { toolInput: { _truncated: true, bytes: sizeOf() }, clipped: [] } : { toolInput: input, clipped: [] };
  const hit = { n: 0 }, clipped = [], out = {};
  for (const [k, v] of Object.entries(input)) { const before = hit.n; out[k] = clipDeep(v, hit); if (hit.n > before) clipped.push(k); }
  if (JSON.stringify(out).length > CAP) return { toolInput: { _truncated: true, bytes: sizeOf() }, clipped: [] };
  return { toolInput: out, clipped };
}

/** The suggestions a row carries (what "Allow and remember" would save): whole, or `{ _truncated, bytes }` when any string in them is
 *  over the cut or the JSON is over the cap. "Allow and remember" saves the panel's own copy, so a rule the card could only show cut
 *  is a rule the card does not offer. */
function shownSuggestions(list) {
  const arr = Array.isArray(list) ? list : [], hit = { n: 0 }, out = clipDeep(arr, hit);
  return hit.n > 0 || JSON.stringify(out).length > CAP ? { _truncated: true, bytes: Buffer.byteLength(JSON.stringify(arr)) } : out;
}

/** A held request as a row shows it: what the card needs, none of the plumbing (the response, the timer, the profile folder). */
export function shown(p) {
  const { toolInput, clipped } = shownInput(p);
  return { id: p.id, open: Boolean(p.open), toolName: p.toolName, toolInput, clipped, suggestions: shownSuggestions(p.suggestions), permissionMode: p.permissionMode ?? null,
           createdAt: p.createdAt ?? null, deadline: p.deadline ?? null, agentId: p.agentId ?? null, agentType: p.agentType ?? null };
}

/** A lookup from a session id to the request to show for it: its main agent's before a subagent's, then the oldest. `of` is how
 *  many the session holds (the card says "1 of 2"; answering one brings the next into the same place). */
export function heldIndex(requests) {
  const by = new Map();
  for (const p of requests) if (p?.sessionId) by.set(p.sessionId, [...(by.get(p.sessionId) ?? []), p]);
  return (sessionId) => {
    const list = by.get(sessionId);
    if (!list) return null;
    const first = [...list].sort((a, b) => Number(Boolean(a.agentId)) - Number(Boolean(b.agentId)) || (a.createdAt ?? 0) - (b.createdAt ?? 0))[0];
    return { ...shown(first), of: list.length };
  };
}

/** Fleet's snapshot with each Claude session of THIS machine carrying `pending`: its held request, else null. A remote
 *  machine's rows get no `pending` key at all: its hooks point at its own panel, where nothing listens, so there is nothing
 *  held for them and a card would promise what cannot happen. */
export function withHeld(snap, held) {
  if (!Array.isArray(snap?.machines)) return snap;
  return { ...snap, machines: snap.machines.map((m) => (m.local !== true ? m : { ...m, accounts: (m.accounts ?? []).map((a) => (a.engine !== 'claude' ? a
    : { ...a, sessions: (a.sessions ?? []).map((x) => ({ ...x, pending: held(x.sessionId) })) })) })) };
}
