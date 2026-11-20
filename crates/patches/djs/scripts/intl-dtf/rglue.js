// rglue.js LOC : the four glues ICU's interval formatter joins a date pattern and a time pattern with
// (full, long, medium, short by month width), as JSON; rangeemit.js reads them. Read off the year-level
// fallback of a date key with hour and minute, between ICU's date pattern (dpat) and time pattern
// (tpatf): they can differ from format's (zh-TW joins with a thin space at every width).
// Run after tpat.js, dpat.js and tpatf.js.
const m = require('./infer.js');
const r = require('./range.js');
const {tmpl} = require('./glue.js');
const TP = JSON.parse(require('fs').readFileSync(__dirname + '/tp-' + m.LOC + '.json'));
const dkey = f => (((f[0] * 4 + f[1]) * 3 + f[2]) * 6 + f[3]) * 3 + f[4];
const tk = ((((1 * 3 + 1) * 3 + 0) * 7 + 4) * 4 + 0) * 4;
const g = (o, f) => {
  const p = r.infer({...o, year: "numeric", hour: "numeric", minute: "numeric", hourCycle: "h23"}, "y", null);
  const v = Array.isArray(p) ? tmpl(p[1], TP.dpat[dkey(f)], TP.tpatf[0][tk]) : null;
  if (v === null) throw new Error("rglue " + m.LOC + " " + JSON.stringify(o) + " " + JSON.stringify(p));
  return v;
};
process.stdout.write(JSON.stringify([g({weekday: "long", month: "long", day: "numeric"}, [1, 0, 1, 3, 1]), g({month: "long", day: "numeric"}, [0, 0, 1, 3, 1]),
  g({month: "short", day: "numeric"}, [0, 0, 1, 4, 1]), g({month: "numeric", day: "numeric"}, [0, 0, 1, 1, 1])]));
