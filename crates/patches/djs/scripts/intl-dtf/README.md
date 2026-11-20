# Intl.DateTimeFormat locale data

`builtins/intl/date/locales.rs` is generated from Node's ICU by probing Intl.DateTimeFormat:

    for l in de fr es ja zh pt-BR; do node infer.js $l > t-$l.json && node gen.js $l && node zn.js $l > z-$l.json; done
    node check.js de        # composes patterns like djs and compares with node on random option sets
    node emit.js . > ../../crates/djs-core/src/builtins/intl/date/locales.rs

zones.tsv is the zone list of data.rs's ZONES (name, POSIX rule), in order.
intl-dtf.jsonl holds parity cases (copy to the parity suites as suites/js/intl-dtf.jsonl);
mkcases.py writes them and matrix.py compares djs with node over options x zones x dates.
