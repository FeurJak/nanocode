// tpatf.js LOC : adds to tp-LOC.json the patterns ICU's fallbacks use for keys with a date and a time:
//   tpatf[z][time key]  the time part of the formatter's own pattern (DTPG over the whole skeleton,
//                       without V8's hour adjustments), read from {day} + the time key at the day level;
//   dpatn[date key]     the date pattern from the normalized date skeleton (fDatePattern), joined by
//                       the medium glue to a fallback time range, read from date key + h:m:s at the hour level.
// Run after range.js, tpat.js and dpat.js.
const m = require('./infer.js');
const r = require('./range.js');
const fs = require('fs');
const {LOC} = m;
const {infer, KINDS} = r;
const {timeOpts} = require('./tpat.js');
const file = __dirname + '/tp-' + LOC + '.json';
const out = JSON.parse(fs.readFileSync(file));
const T = JSON.parse(fs.readFileSync(__dirname + '/t-' + LOC + '.json'));
const {tmpl} = require('./glue.js');
const dk = f => (((f[0] * 4 + f[1]) * 3 + f[2]) * 6 + f[3]) * 3 + f[4];
const t0 = T.time[((((1 * 3 + 1) * 3 + 0) * 7 + 4) * 4 + 0) * 4];
const gs = tmpl(m.infer({month: "numeric", day: "numeric", hour: "numeric", minute: "numeric", hourCycle: "h23"}).p, T.date[dk([0, 0, 0, 1, 1])], t0);
const gm = out.gm;
const sep = JSON.parse(fs.readFileSync(__dirname + '/r-' + LOC + '.json')).sep;
const q = lit => /[A-Za-z']/.test(lit) ? "'" + lit.replace(/'/g, "''") + "'" : lit;
// strip a known date pattern D and glue g from P = apply(g, D, X), giving X
function strip(P, g, D) {
  if (!g.includes("{")) return P.startsWith(D + g) ? P.slice((D + g).length) : null;
  const i0 = g.indexOf("{0}"), i1 = g.indexOf("{1}");
  const pre = g.slice(0, Math.min(i0, i1)), mid = g.slice(Math.min(i0, i1) + 3, Math.max(i0, i1)), post = g.slice(Math.max(i0, i1) + 3);
  if (!P.startsWith(pre) || !P.endsWith(post)) return null;
  const body = P.slice(pre.length, P.length - post.length);
  if (i1 < i0) return body.startsWith(D + mid) ? body.slice((D + mid).length) : null;
  return body.endsWith(mid + D) ? body.slice(0, body.length - (mid + D).length) : null;
}
const bad = [];
// a fallback pair: the first half is the second plus the separator (however the quotes fall)
const unq = P => P.replace(/'([^']*)'/g, (_, x) => x === "" ? "'" : x);
const isFb = p => unq(p[0]) === unq(p[1]) + sep;
out.tpatf = [{}, {}, {}, {}, {}, {}, {}];
const Dd = T.date[dk([0, 0, 0, 0, 1])];
for (let k = 0; k < 3024; k++) {
  const o = timeOpts(k); if (!o) continue;
  for (let z = 0; z < 7; z++) {
    const oz = z === 0 ? {...o, day: "numeric"} : {...o, day: "numeric", timeZoneName: KINDS[z - 1]};
    const p = infer(oz, "d", null);
    const X = Array.isArray(p) && isFb(p) ? strip(p[1], gs, Dd) : null;
    if (X === null) { bad.push([oz, p]); continue; }
    out.tpatf[z][k] = X;
  }
}
// normalized date pattern next to a fallback time range
out.dpatn = {};
const tf = out.tpat[0][((((1 * 3 + 1) * 3 + 1) * 7 + 4) * 4 + 0) * 4];
for (let k = 0; k < 864; k++) {
  const o = r.dateOpts(k); if (!o) continue;
  const p = infer({...o, hour: "numeric", minute: "numeric", second: "numeric", hourCycle: "h23"}, "h", null);
  let D = null;
  if (Array.isArray(p) && p[1] === tf || (Array.isArray(p) && gm.includes("{"))) {
    const a = tf + q(sep);
    if (!gm.includes("{")) { if (p[0].endsWith(gm + a) && p[1] === tf) D = p[0].slice(0, -(gm + a).length); }
    else {
      const i0 = gm.indexOf("{0}"), i1 = gm.indexOf("{1}");
      if (i1 < i0) { const pre = gm.slice(0, i1), mid = gm.slice(i1 + 3, i0); if (p[0].startsWith(pre) && p[0].endsWith(mid + a) && p[1] === tf + gm.slice(i0 + 3)) D = p[0].slice(pre.length, p[0].length - mid.length - a.length); }
      else { const pre = gm.slice(0, i0), mid = gm.slice(i0 + 3, i1), post = gm.slice(i1 + 3); if (p[0] === pre + a && p[1].startsWith(tf + mid) && p[1].endsWith(post)) D = p[1].slice(tf.length + mid.length, p[1].length - post.length); }
    }
  }
  if (D === null) { bad.push([o, "dpatn", p]); continue; }
  out.dpatn[k] = D;
}
out.badf = bad;
fs.writeFileSync(file, JSON.stringify(out));
console.log(LOC, "tpatf/dpatn bad", bad.length, JSON.stringify(gs));
for (const b of bad.slice(0, 4)) console.log(JSON.stringify(b).slice(0, 300));

