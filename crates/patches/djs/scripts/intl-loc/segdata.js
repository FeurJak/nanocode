// Generates crates/djs-core/src/builtins/intl/segdata.rs (Intl.Segmenter's character
// properties) from Unicode 17's UCD (node 22's ICU 78 is Unicode 17):
//   node segdata.js CLDR > ../../crates/djs-core/src/builtins/intl/segdata.rs
// CLDR/ucd holds GraphemeBreakProperty, WordBreakProperty, SentenceBreakProperty,
// emoji-data, DerivedCoreProperties (InCB), Scripts and LineBreak (fetch.sh).
// One table of ranges: RANGES[i] = (first code point << 8) | i into PROPS; PROPS[i] packs
// gcb (4 bits) | ep << 4 | incb << 5 (2) | wb << 7 (5) | sb << 12 (4) | class << 16 (3).
const fs = require("fs"), path = require("path");
const DIR = path.join(process.argv[2], "ucd");
const GCB = ["Other", "CR", "LF", "Control", "Extend", "ZWJ", "Regional_Indicator", "Prepend", "SpacingMark", "L", "V", "T", "LV", "LVT"];
const WB = ["Other", "CR", "LF", "Newline", "Extend", "ZWJ", "Regional_Indicator", "Format", "Katakana", "Hebrew_Letter", "ALetter", "Single_Quote", "Double_Quote", "MidNumLet", "MidLetter", "MidNum", "Numeric", "ExtendNumLet", "WSegSpace"];
const SB = ["Other", "CR", "LF", "Extend", "Sep", "Format", "Sp", "Lower", "Upper", "OLetter", "Numeric", "ATerm", "SContinue", "STerm", "Close"];
const INCB = ["None", "Linker", "Consonant", "Extend"];
// class: 0 none, 1 ideographic (Han), 2 kana (Hiragana), 3 complex (Line_Break=SA),
// 4 Hangul syllable (ICU's word rules join these only with each other).
const N = 0x110000;
const fields = { gcb: new Uint8Array(N), ep: new Uint8Array(N), incb: new Uint8Array(N), wb: new Uint8Array(N), sb: new Uint8Array(N), cls: new Uint8Array(N) };
const load = (file, fn) => {
  for (const line of fs.readFileSync(path.join(DIR, file), "utf8").split("\n")) {
    const l = line.split("#")[0].trim();
    if (!l) continue;
    const parts = l.split(";").map((s) => s.trim());
    const [a, b] = parts[0].split("..").map((x) => parseInt(x, 16));
    for (let c = a; c <= (b ?? a); c++) fn(c, parts.slice(1));
  }
};
const idx = (list, v, file) => { const i = list.indexOf(v); if (i < 0) throw new Error(file + ": " + v); return i; };
load("GraphemeBreakProperty.txt", (c, [v]) => (fields.gcb[c] = idx(GCB, v, "gcb")));
load("emoji-data.txt", (c, [v]) => { if (v === "Extended_Pictographic") fields.ep[c] = 1; });
load("DerivedCoreProperties.txt", (c, [p, v]) => { if (p === "InCB") fields.incb[c] = idx(INCB, v, "incb"); });
load("WordBreakProperty.txt", (c, [v]) => (fields.wb[c] = idx(WB, v, "wb")));
load("SentenceBreakProperty.txt", (c, [v]) => (fields.sb[c] = idx(SB, v, "sb")));
load("Scripts.txt", (c, [v]) => { if (v === "Han") fields.cls[c] = 1; else if (v === "Hiragana") fields.cls[c] = 2; });
load("LineBreak.txt", (c, [v]) => { if (v === "SA" && !fields.cls[c]) fields.cls[c] = 3; if (v === "ID" && !fields.cls[c] && /\p{Ideographic}/u.test(String.fromCodePoint(c))) fields.cls[c] = 1; });
for (let c = 0xac00; c <= 0xd7a3; c++) fields.cls[c] = 4;
const pack = (c) => fields.gcb[c] | (fields.ep[c] << 4) | (fields.incb[c] << 5) | (fields.wb[c] << 7) | (fields.sb[c] << 12) | (fields.cls[c] << 16);
const props = [], pidx = new Map(), ranges = [];
let last = -1;
for (let c = 0; c < N; c++) {
  const p = pack(c);
  if (p === last) continue;
  last = p;
  if (!pidx.has(p)) { pidx.set(p, props.length); props.push(p); }
  ranges.push(c * 256 + pidx.get(p));
}
if (props.length > 256) throw new Error("too many property sets: " + props.length);
const lines = (arr, per) => { const o = []; for (let i = 0; i < arr.length; i += per) o.push("    " + arr.slice(i, i + per).join(", ") + ","); return o.join("\n"); };
process.stdout.write("// Made by crates/patches/djs/scripts/intl-loc/segdata.js from Unicode 17's UCD (node " + process.version + ", ICU " + process.versions.icu + ", Unicode " + process.versions.unicode + "): do not edit.\n#![cfg_attr(rustfmt, rustfmt::skip)]\n\n" +
  "/// The distinct property sets (see segdata.js).\npub(super) static PROPS: [u32; " + props.length + "] = [\n" + lines(props, 12) + "\n];\n\n" +
  "/// (first code point << 8) | index into PROPS, sorted.\npub(super) static RANGES: [u32; " + ranges.length + "] = [\n" + lines(ranges, 10) + "\n];\n");
