# Intl.DateTimeFormat locale data

`builtins/intl/date/locales.rs` is generated from Node's ICU by probing Intl.DateTimeFormat:

    L="de fr es ja zh pt-BR ko it ru nl pl tr ar hi id vi zh-TW en-GB en-IN es-MX fr-CA"
    for l in $L; do node infer.js $l > t-$l.json && node gen.js $l && node zn.js $l > z-$l.json; done
    node check.js de        # composes patterns like djs and compares with node on random option sets
    node emit.js . > ../../crates/djs-core/src/builtins/intl/date/locales.rs
    node alias.js ar ar-EG  # checks that an alias formats exactly like its locale

emit.js lists the locales (LOCS) and the tags each answers to (ALIASES). A tag that isn't listed
uses its bare language's data (it-CH formats as it), and zh tags with Hant, TW, HK or MO use zh-TW.
Arabic regions whose node default is numberingSystem arab (ar-EG, ar-SA and others) format with
ar's data and Arabic-Indic digits, which date.rs substitutes after formatting.

A glue (glue.js) joins a date pattern and a time pattern: either the literal between them, or a
template with {1} for the date and {0} for the time when the time comes first or the pair is
wrapped (hi, vi).

To keep the binary small, the four pattern tables of every locale are cut into rows of 16 cells
stored once in DTF_ROWS, and the pattern and zone-name pools are each one text with end offsets.

zones.tsv is the zone list of data.rs's ZONES (name, POSIX rule), in order.
intl-dtf.jsonl holds parity cases (copy to the parity suites as suites/js/intl-dtf.jsonl);
mkcases.py FILE writes them and matrix.py compares djs with node over options x zones x dates.

range.js LOC (after infer.js LOC > t-LOC.json) writes r-LOC.json: node's formatRange interval
patterns for every date key (era, year, month and day levels), every time key without seconds
(am/pm, hour and minute levels, plain and with each timeZoneName), and the date and time styles,
each cut where the end date starts and checked against formatRangeToParts on three date pairs.
Under node v22.23.5 all 22 locales infer with no mismatches except it (96 era-level keys). djs has
no formatRange yet: the table is about 11k distinct pattern pairs, so the port should implement
ICU's DateIntervalFormat over CLDR 48 intervalFormats (node 22.23.5 runs ICU 78.3) and use these
tables to check it rather than embed them.
