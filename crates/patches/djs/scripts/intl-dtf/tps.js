// tps.js LOC : adds to tp-LOC.json the pattern ICU's fallback uses at a time level for a time-only key
// that has no interval data (seconds, fractional seconds): tps[z][time key], only where it is not
// tpat's (the day level's). ICU formats it with its own pattern for the skeleton, which for an hour
// with seconds and no minute appends the hour to the seconds (vi "ss (Giờ: HH 'giờ')"), unlike the
// pattern it joins to a date. Run after tpat.js.
const m = require('./infer.js');
const r = require('./range.js');
const fs = require('fs');
const {LOC} = m;
const {infer, KINDS} = r;
const {timeOpts} = require('./tpat.js');
const file = __dirname + '/tp-' + LOC + '.json';
const out = JSON.parse(fs.readFileSync(file));
const sep = JSON.parse(fs.readFileSync(__dirname + '/r-' + LOC + '.json')).sep;
const q = lit => /[A-Za-z']/.test(lit) ? "'" + lit.replace(/'/g, "''") + "'" : lit;
out.tps = [{}, {}, {}, {}, {}, {}, {}];
let n = 0;
const bad = [];
for (let k = 0; k < 3024; k++) {
  const o = timeOpts(k); if (!o) continue;
  if (!o.second && !o.fractionalSecondDigits) continue;
  for (let z = 0; z < 7; z++) {
    const oz = z === 0 ? o : {...o, timeZoneName: KINDS[z - 1]};
    const lvs = o.second ? ["s", "h"] : o.minute ? ["m", "h"] : ["h"];
    let p = null;
    for (const lv of lvs) { p = infer(oz, lv, null); if (Array.isArray(p)) break; }
    if (!Array.isArray(p)) { bad.push([oz, p]); continue; }
    if (p[0] !== p[1] + q(sep) && p[0].replace(/'/g, "") !== (p[1] + sep).replace(/'/g, "")) { bad.push([oz, "notfb", p]); continue; }
    if (p[1] !== out.tpat[z][k]) { out.tps[z][k] = p[1]; n++; }
  }
}
out.badtps = bad;
fs.writeFileSync(file, JSON.stringify(out));
console.log(LOC, "tps differ", n, "bad", bad.length);
for (const b of bad.slice(0, 4)) console.log(JSON.stringify(b).slice(0, 300));
