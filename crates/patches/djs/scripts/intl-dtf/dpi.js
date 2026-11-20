// dpi.js LOC : adds to tp-LOC.json the date pattern of a fallback at a date level the key lacks
// (dpi[dkey * 4 + level], only where not dpat's for the widened key). ICU's DateIntervalFormat inserts
// the missing field (d, then M, y, G) into the whole skeleton and formats with its best pattern, which
// can differ from the date pattern it joins to a time range (zh-TW M月d日 vs M/d, pt-BR d/M/y vs
// dd/MM/y). Measured with hour and minute (h23), split off with rglue's glue by month width.
// Run after tpat.js, dpat.js and tpatf.js.
const m = require('./infer.js');
const r = require('./range.js');
const fs = require('fs');
const {execFileSync} = require('child_process');
const {LOC} = m;
const file = __dirname + '/tp-' + LOC + '.json';
const out = JSON.parse(fs.readFileSync(file));
const glues = JSON.parse(execFileSync(process.execPath, [__dirname + '/rglue.js', LOC], {encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore']}));
const dk = f => (((f[0] * 4 + f[1]) * 3 + f[2]) * 6 + f[3]) * 3 + f[4];
const unk = k => { const f = []; for (const n of [3, 6, 3, 4]) { f.unshift(k % n); k = Math.floor(k / n); } f.unshift(k); return f; };
const tk = ((((1 * 3 + 1) * 3 + 0) * 7 + 4) * 4 + 0) * 4;
const T = out.tpatf[0][tk];
const sep = JSON.parse(fs.readFileSync(__dirname + '/r-' + LOC + '.json')).sep;
const q = lit => /[A-Za-z']/.test(lit) ? "'" + lit.replace(/'/g, "''") + "'" : lit;
const U = p => r.toks(p).flatMap(t => /^[A-Za-z]/.test(t) ? [t] : [...(t[0] === "'" ? (t === "''" ? "'" : t.slice(1, -1).replace(/''/g, "'")) : t)].map(c => "\0" + c));
const J = us => { let o = "", lit = ""; for (const u of us) { if (u[0] === "\0") lit += u.slice(1); else { o += q(lit) + u; lit = ""; } } return o + q(lit); };
const eq = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
// date units of P = glue with {1} the date and {0} the time T
const strip = (g, P, Tp = T) => {
  if (!g.includes("{")) g = "{1}" + g + "{0}";
  const b = U(P), t = U(Tp), i0 = g.indexOf("{0}"), i1 = g.indexOf("{1}");
  const parts = (i1 < i0 ? [g.slice(0, i1), g.slice(i1 + 3, i0), g.slice(i0 + 3)] : [g.slice(0, i0), g.slice(i0 + 3, i1), g.slice(i1 + 3)]).map(U);
  const [pre, mid, post] = parts;
  const head = i1 < i0 ? pre : [...pre, ...t, ...mid];
  const tail = i1 < i0 ? [...mid, ...t, ...post] : post;
  if (b.length < head.length + tail.length) return null;
  if (!eq(b.slice(0, head.length), head) || !eq(b.slice(b.length - tail.length), tail)) return null;
  return J(b.slice(head.length, b.length - tail.length));
};
const glueBy = g => g[3] === 3 ? (g[0] !== 0 ? glues[0] : glues[1]) : g[3] === 4 ? glues[2] : glues[3];
const bad = [];
let n = 0;
const run = (TO, Tp, dst) => {
for (let k = 0; k < 864; k++) {
  const o = r.dateOpts(k); if (!o) continue;
  const f = unk(k);
  if (dk(f) !== k) throw new Error("unk");
  for (let lv = 0; lv < 4; lv++) {
    const single = m.infer({...o, ...TO}).p;
    const missing = f[lv + 1] === 0 || (lv === 2 && r.toks(single).some(t => t[0] === "L"));
    if (!missing) continue;
    const g = [...f];
    for (const [l2, i, v] of [[3, 4, 1], [2, 3, 1], [1, 2, 1], [0, 1, 2]]) if (lv <= l2 && g[i] === 0) g[i] = v;
    const p = r.infer({...o, ...TO}, ["G", "y", "M", "d"][lv], null);
    if (!Array.isArray(p)) { bad.push([o, lv, p]); continue; }
    if (p[0] !== p[1] + q(sep) && p[0].replace(/'/g, "") !== (p[1] + sep).replace(/'/g, "")) { bad.push([o, lv, "notfb", p]); continue; }
    const D = strip(glueBy(g), p[1], Tp);
    if (D === null) { bad.push([o, lv, "strip", p[1], glueBy(g)]); continue; }
    if (D !== out.dpat[dk(g)]) { dst[k * 4 + lv] = D; n++; }
  }
}
};
out.dpi = {}; out.dpih = {};
run({hour: "numeric", minute: "numeric", hourCycle: "h23"}, T, out.dpi);
// keys with an hour and no minute: ICU's best pattern for the whole skeleton can pick another date
// form (ja "y/M/dE H時" where with minutes it is "y/M/d(E) H:mm")
const tkh = ((((1 * 3 + 0) * 3 + 0) * 7 + 4) * 4 + 0) * 4;
run({hour: "numeric", hourCycle: "h23"}, out.tpatf[0][tkh], out.dpih);
// and with a 12-hour cycle, whose day period changes the skeleton (ko "(E)" where h23 has "E")
out.dpih12 = {};
const tkh12 = ((((1 * 3 + 0) * 3 + 0) * 7 + 3) * 4 + 0) * 4;
run({hour: "numeric", hourCycle: "h12"}, out.tpatf[0][tkh12], out.dpih12);
// time-only keys (date key 0): the date part ICU joins at a date level, which can keep the era width
// of the locale's GyMd pattern (ru "dd.MM.y GGGGG") where the option bag's era is short.
for (let lv = 0; lv < 4; lv++) {
  const p = r.infer({hour: "numeric", minute: "numeric", hourCycle: "h23"}, ["G", "y", "M", "d"][lv], null);
  if (!Array.isArray(p)) { bad.push([{}, lv, p]); continue; }
  const D = strip(glues[3], p[1]);
  const fd = lv === 0 ? [0, 2, 1, 1, 1] : [0, 0, 1, 1, 1];
  if (D === null) { bad.push([{}, lv, "strip", p[1], glues[3]]); continue; }
  if (D !== out.dpat[dk(fd)]) { out.dpi[lv] = D; n++; }
}
out.baddpi = bad;
fs.writeFileSync(file, JSON.stringify(out));
console.log(LOC, "dpi differ", n, "bad", bad.length);
for (const b of bad.slice(0, 4)) console.log(JSON.stringify(b).slice(0, 300));
