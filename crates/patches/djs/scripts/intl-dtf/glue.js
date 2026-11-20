// glue.js: how a locale joins a date pattern and a time pattern.
// A glue is either the literal between them ("' at '") or, when the time comes first or the
// locale wraps the pair (hi "{1}' को '{0}' बजे'", vi "{0} {1}"), a template: {1} = date, {0} = time.
// The placeholders sit outside quoted literals, so a plain glue never contains them.
// units: a field run, or one literal character (quoting can merge a pattern's own literal into the
// glue's: fi "d.M'. klo 'H.mm" is "d.M." + "' klo '" + "H.mm")
const units = p => { const o = []; for (let i = 0; i < p.length;) { const c = p[i];
  if (c === "'") { if (p[i + 1] === "'") { o.push("\0'"); i += 2; continue; } let j = i + 1, s = "";
    for (; j < p.length; j++) { if (p[j] === "'") { if (p[j + 1] === "'") { s += "'"; j++; } else break; } else s += p[j]; }
    for (const ch of s) o.push("\0" + ch); i = j + 1; continue; }
  if (/[A-Za-z]/.test(c)) { let j = i; while (p[j] === c) j++; o.push(p.slice(i, j)); i = j; continue; }
  o.push("\0" + c); i++; } return o; };
const q = lit => lit === "" ? "" : /[A-Za-z']/.test(lit) ? "'" + lit.replace(/'/g, "''") + "'" : lit;
const join = us => { let o = "", lit = ""; for (const u of us) { if (u[0] === "\0") lit += u.slice(1); else { o += q(lit) + u; lit = ""; } } return o + q(lit); };
const eq = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
const tmplUnits = (p, dp, tp) => {
  const P = units(p), A = units(dp), B = units(tp);
  if (P.length >= A.length + B.length && eq(P.slice(0, A.length), A) && eq(P.slice(P.length - B.length), B)) {
    const mid = join(P.slice(A.length, P.length - B.length));
    if (!mid.includes("{")) return mid;
  }
  return null;
};
const tmpl = (p, dp, tp) => {
  const v = tmpl0(p, dp, tp);
  return v === null ? tmplUnits(p, dp, tp) : v;
};
const tmpl0 = (p, dp, tp) => {
  if (p.length >= dp.length + tp.length && p.startsWith(dp) && p.endsWith(tp)) {
    const mid = p.slice(dp.length, p.length - tp.length);
    if (!mid.includes("{")) return mid;
  }
  const i = p.indexOf(dp);
  if (i < 0) return null;
  let j = -1;
  for (let k = p.indexOf(tp); k >= 0; k = p.indexOf(tp, k + 1)) if (k + tp.length <= i || k >= i + dp.length) { j = k; break; }
  if (j < 0) return null;
  return j > i ? p.slice(0, i) + "{1}" + p.slice(i + dp.length, j) + "{0}" + p.slice(j + tp.length)
               : p.slice(0, j) + "{0}" + p.slice(j + tp.length, i) + "{1}" + p.slice(i + dp.length);
};
const apply = (g, d, t) => g.includes("{") ? g.replace("{1}", () => d).replace("{0}", () => t) : d + g + t;
module.exports = {tmpl, apply};
