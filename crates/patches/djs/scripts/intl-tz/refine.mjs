// Split probe.mjs's periods where a locale's generic zone names change inside them (ICU compares a zone
// with its metazone's golden zone for the locale's region at each instant, and uses the standard name
// far from DST), so each period has one set of names in every locale:
// node refine.mjs PROBE.json OUT.json [shard nshards]. OUT has PROBE's shape; split-off periods repeat
// their parent's offset, dst and en names. Feed OUT to probe-loc.mjs (PROBE_ZONES limits it to the zones
// that changed) and to encode.py as PROBE.
import fs from "node:fs";
import { LOCALES } from "./probe-loc.mjs";
// REFINE_STEP_DAYS (default 30) is the sampling step: a change that reverts within one step can be missed
const STEP = (+process.env.REFINE_STEP_DAYS || 30) * 86400, END = 4133980800; // 2101
const [pin, out, shard = "0", nsh = "1"] = process.argv.slice(2);
const probe = JSON.parse(fs.readFileSync(pin));
const res = {};
for (const z of Object.keys(probe).filter((_, i) => i % +nsh === +shard)) {
  // the names alone: format() would add the date, which changes at every step
  const fs2 = LOCALES.flatMap(L => ["shortGeneric", "longGeneric"].map(k => new Intl.DateTimeFormat(L, { timeZone: z, timeZoneName: k, year: "numeric" })));
  const sig = t => fs2.map(f => f.formatToParts(t * 1000).find(x => x.type === "timeZoneName").value).join("|");
  const P = probe[z], R = [];
  P.forEach((p, j) => {
    R.push(p);
    const end = Math.min(j + 1 < P.length ? P[j + 1][0] : END, END);
    let a = p[0], sa = sig(a);
    while (a < end - 1) {
      const b = Math.min(a + STEP, end - 1), sb = sig(b);
      if (sb === sa) { a = b; continue; }
      let lo = a, hi = b; // sig(lo) == sa, sig(hi) != sa
      while (hi - lo > 1) { const m = Math.floor((lo + hi) / 2); if (sig(m) === sa) lo = m; else hi = m; }
      R.push([hi, p[1], p[2], p[3]]);
      a = hi; sa = sig(hi);
    }
  });
  // drop splits that last under a day (the hour after a fall-back, where ICU's names flip briefly):
  // the period before keeps those instants
  const own = new Set(P.map(p => p[0]));
  res[z] = R.filter((p, j) => own.has(p[0]) || (j + 1 < R.length ? R[j + 1][0] : END) - p[0] >= 86400);
}
fs.writeFileSync(out, JSON.stringify(res));
