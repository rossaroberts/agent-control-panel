// The requests the panel is holding, laid over Fleet's rows. Pure: the snapshot goes in, a copy comes out (the snapshot
// is Fleet's cache, shared by every page, so it is never written to). Held requests live on the Mac's panel, in server.mjs;
// fleet.mjs stays free of them.

/** The longest text a row carries from a held request's input (a Write's whole file, say, would ride every poll). */
const CLIP = 4000;
/** Questions and plans are read whole: the card needs every option and the plan. */
const WHOLE = new Set(['AskUserQuestion', 'ExitPlanMode']);

/** A held request as a row shows it: what the card needs, none of the plumbing (the response, the timer, the profile folder). */
export function shown(p) {
  const input = p.toolInput && typeof p.toolInput === 'object' ? p.toolInput : {};
  const toolInput = WHOLE.has(p.toolName) ? input
    : Object.fromEntries(Object.entries(input).map(([k, v]) => [k, typeof v === 'string' && v.length > CLIP ? v.slice(0, CLIP) : v]));
  return { id: p.id, open: Boolean(p.open), toolName: p.toolName, toolInput, suggestions: p.suggestions ?? [], permissionMode: p.permissionMode ?? null,
           createdAt: p.createdAt ?? null, deadline: p.deadline ?? null, agentId: p.agentId ?? null, agentType: p.agentType ?? null };
}

/** A lookup from a session id to the request to show for it: its main agent's before a subagent's, then the oldest. */
export function heldIndex(requests) {
  const by = new Map();
  for (const p of requests) if (p?.sessionId) by.set(p.sessionId, [...(by.get(p.sessionId) ?? []), p]);
  return (sessionId) => {
    const list = by.get(sessionId);
    if (!list) return null;
    const first = [...list].sort((a, b) => Number(Boolean(a.agentId)) - Number(Boolean(b.agentId)) || (a.createdAt ?? 0) - (b.createdAt ?? 0))[0];
    return shown(first);
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
