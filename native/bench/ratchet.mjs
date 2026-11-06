// The pure parts of the bench gate: the median, and the ratchet rule (Global Constraints, Ruling R19).
export const round = (n) => Math.round(n * 100) / 100;

export const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};

/** The budgets after a ratchet: each key with a measured median at or under its budget becomes
 *  min(budget, max(median x 1.25, floor)). A key over budget, or with no median, is left alone (a failure to fix is not a
 *  number to adopt). Returns { budgets, skipped }; `budgets` is new, the argument is not touched. */
export function ratchet(budgets, medians) {
  const out = {}, skipped = [];
  for (const [key, b] of Object.entries(budgets)) {
    const m = medians[key];
    if (m == null || m > b.budget) { out[key] = { ...b }; if (m != null) skipped.push(key); continue; }
    out[key] = { ...b, budget: round(Math.min(b.budget, Math.max(m * 1.25, b.floor))) };
  }
  return { budgets: out, skipped };
}
