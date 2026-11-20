// ECMA-402 (Intl): the constructors read their options here, in the order the spec
// reads them, into a raw option list; builtins/intl (Rust) resolves it against the locale
// data into the resolved options and formats with them. Both lists are strings of
// name \u0002 value pairs joined by \u0001; a resolved value is a string, or a number
// (#), a boolean (!t, !f) or a list (@a,b). en-US matches node; a few other locales have
// their number, list and relative time data. Messages are node's. Builds without
// djs-core's \`embed\` have no locale data: IntlEnabled is false there, and the
// toLocaleString methods keep their locale-free results.

/** CanonicalizeLocaleList's input: the language tags, joined by LF. */
function IntlLocaleTags(locales: Value): Str {
  if (locales === undefined) {
    return "";
  }
  if (IsString(locales)) {
    return AsStr(locales);
  }
  const o: Obj = ToObject(locales);
  const len: f64 = LengthOfArrayLike(o);
  let out: Str = "";
  let i: f64 = 0;
  while (i < len) {
    if (HasProperty(o, i)) {
      const v: Value = Get(o, i);
      if (!IsString(v) && !IsObject(v)) {
        throw NewTypeError("Language ID should be string or object.");
      }
      out = out === "" ? ToString(v) : out + "\n" + ToString(v);
    }
    i += 1;
  }
  return out;
}

/** The items of a list joined by sep, as an array. */
function IntlSplit(s: Str, sep: Str, from: f64): Obj {
  const out: Obj = ArrayCreate(0);
  let start: f64 = from;
  const len: f64 = StrLength(s);
  while (start < len) {
    let end: f64 = StrIndexOf(s, sep, start);
    if (end < 0) {
      end = len;
    }
    ArrayPushDense(out, StrSubstring(s, start, end));
    start = end + 1;
  }
  return out;
}

/**
 * The raw option list of options: each of names (name:kind, joined by commas; kind s for
 * a string, b a boolean, n a number, g useGrouping's boolean or string) that is not
 * undefined, converted.
 */
function IntlReadOptions(options: Value, names: Str): Str {
  if (options === undefined) {
    return "";
  }
  const o: Obj = ToObject(options);
  let out: Str = "";
  let start: f64 = 0;
  const len: f64 = StrLength(names);
  while (start < len) {
    let end: f64 = StrIndexOf(names, ",", start);
    if (end < 0) {
      end = len;
    }
    const kind: Str = StrSubstring(names, end - 1, end);
    const name: Str = StrSubstring(names, start, end - 2);
    start = end + 1;
    const v: Value = Get(o, name);
    if (v === undefined) {
      continue;
    }
    let s: Str = "";
    if (kind === "b") {
      s = ToBoolean(v) ? "true" : "false";
    } else if (kind === "n") {
      const n: f64 = ToNumber(v);
      if (n !== n) {
        throw NewRangeError(name + " value is out of range.");
      }
      s = NumberToString(n, 10);
    } else if (kind === "g") {
      if (v === true) {
        s = "true";
      } else if (!ToBoolean(v)) {
        s = "false";
      } else {
        s = ToString(v);
        if (s === "true" || s === "false") {
          continue;
        }
      }
    } else {
      s = ToString(v);
    }
    out = IntlWith(out, name, s);
  }
  return out;
}

/** raw with one more pair. */
function IntlWith(raw: Str, name: Str, v: Str): Str {
  return raw === "" ? name + "\u0002" + v : raw + "\u0001" + name + "\u0002" + v;
}

/** The value of a resolved option. */
function IntlValue(v: Str): Value {
  if (v === "!t") {
    return true;
  }
  if (v === "!f") {
    return false;
  }
  const c: f64 = StrLength(v) > 0 ? StrCharCodeAt(v, 0) : 0;
  if (c === 35) {
    return ToNumber(StrSubstring(v, 1, StrLength(v)));
  }
  if (c === 64) {
    return IntlSplit(v, ",", 1);
  }
  return v;
}

/** The resolved options object of a resolved option list (internal names start with "at"). */
function IntlResolvedObject(spec: Str): Obj {
  const o: Obj = ObjectCreate(RealmIntrinsic("ObjectPrototype"));
  let start: f64 = 0;
  const len: f64 = StrLength(spec);
  while (start < len) {
    let end: f64 = StrIndexOf(spec, "\u0001", start);
    if (end < 0) {
      end = len;
    }
    const mid: f64 = StrIndexOf(spec, "\u0002", start);
    const name: Str = StrSubstring(spec, start, mid);
    if (StrCharCodeAt(name, 0) !== 64) {
      CreateDataPropertyOrThrow(o, name, IntlValue(StrSubstring(spec, mid + 1, end)));
    }
    start = end + 1;
  }
  return o;
}

/** formatToParts's array of { type, value } (and unit, for a relative time's number). */
function IntlParts(encoded: Value, unit: Value): Obj {
  const s: Str = AsStr(encoded);
  const out: Obj = ArrayCreate(0);
  let start: f64 = 0;
  const len: f64 = StrLength(s);
  while (start < len) {
    let end: f64 = StrIndexOf(s, "\u0001", start);
    if (end < 0) {
      end = len;
    }
    const mid: f64 = StrIndexOf(s, "\u0002", start);
    const part: Obj = ObjectCreate(RealmIntrinsic("ObjectPrototype"));
    const ty: Str = StrSubstring(s, start, mid);
    CreateDataPropertyOrThrow(part, "type", ty);
    CreateDataPropertyOrThrow(part, "value", StrSubstring(s, mid + 1, end));
    if (unit !== undefined && ty !== "literal") {
      CreateDataPropertyOrThrow(part, "unit", unit);
    }
    ArrayPushDense(out, part);
    start = end + 1;
  }
  return out;
}

/** The resolved options of an Intl object of kind, else TypeError naming method. */
function IntlThis(v: Value, kind: i32, method: Str): Str {
  const spec: Value = IntlSpec(v, kind);
  if (spec === undefined) {
    let shown: Str = "#<Object>";
    if (!IsObject(v)) {
      shown = IsSymbol(v) ? "Symbol()" : ToString(v);
    }
    throw NewTypeError("Method " + method + " called on incompatible receiver " + shown);
  }
  return AsStr(spec);
}

/** Intl[name]. */
function IntlConstructor(name: Str): Obj {
  return AsObj(Get(AsObj(Get(GlobalObject(), "Intl")), name));
}

/** A new Intl object of kind for locales and the raw options. */
function IntlConstruct(newTarget: Value, name: Str, kind: i32, locales: Value, raw: Str): Obj {
  const ctor: Obj = IntlConstructor(name);
  let proto: Value = Get(AsObj(newTarget === undefined ? ctor : newTarget), "prototype");
  if (!IsObject(proto)) {
    proto = Get(ctor, "prototype");
  }
  return IntlCreate(AsObj(proto), kind, IntlResolve(kind, IntlLocaleTags(locales), raw));
}

/** The TypeError of an Intl constructor called without new. */
function IntlRequireNew(newTarget: Value, name: Str): void {
  if (newTarget === undefined) {
    throw NewTypeError("Constructor Intl." + name + " requires 'new'");
  }
}

/**
 * The bound format function of a NumberFormat or DateTimeFormat: \`this\` is the format.
 * @intrinsic
 */
function IntlBoundFormat(this: Value, x: Value): Str {
  const spec: Value = IntlSpec(this, 0);
  if (spec !== undefined) {
    return AsStr(IntlFormat(0, AsStr(spec), IntlNumeric(x), undefined, false));
  }
  return AsStr(IntlFormat(1, AsStr(IntlSpec(this, 1)), IntlDateValue(x), undefined, false));
}

/**
 * The bound compare function of a Collator: \`this\` is the collator.
 * @intrinsic
 */
function IntlBoundCompare(this: Value, x: Value, y: Value): f64 {
  return AsNumber(IntlFormat(2, AsStr(IntlSpec(this, 2)), ToString(x), ToString(y), false));
}

/** The cached bound function (closure kind) of an Intl object. */
function IntlBoundFunction(o: Value, kind: i32, length: i32): Value {
  const cached: Value = IntlBound(AsObj(o), undefined);
  if (cached !== undefined) {
    return cached;
  }
  return IntlBound(AsObj(o), NewBuiltinClosure(kind, o, length));
}

/** The options its constructor reads. */
function IntlNfOptions(): Str {
  return "localeMatcher:s,numberingSystem:s,style:s,currency:s,currencyDisplay:s,currencySign:s,unit:s,unitDisplay:s,notation:s,minimumIntegerDigits:n,minimumFractionDigits:n,maximumFractionDigits:n,minimumSignificantDigits:n,maximumSignificantDigits:n,roundingIncrement:n,roundingMode:s,roundingPriority:s,trailingZeroDisplay:s,compactDisplay:s,useGrouping:g,signDisplay:s";
}
/** The options its constructor reads. */
function IntlDtfOptions(): Str {
  return "localeMatcher:s,calendar:s,numberingSystem:s,hour12:b,hourCycle:s,timeZone:s,weekday:s,era:s,year:s,month:s,day:s,dayPeriod:s,hour:s,minute:s,second:s,fractionalSecondDigits:n,timeZoneName:s,formatMatcher:s,dateStyle:s,timeStyle:s";
}
/** The options its constructor reads. */
function IntlCollatorOptions(): Str {
  return "usage:s,localeMatcher:s,collation:s,numeric:b,caseFirst:s,sensitivity:s,ignorePunctuation:b";
}
/** The options its constructor reads. */
function IntlPluralOptions(): Str {
  return "localeMatcher:s,type:s,minimumIntegerDigits:n,minimumFractionDigits:n,maximumFractionDigits:n,minimumSignificantDigits:n,maximumSignificantDigits:n,roundingIncrement:n,roundingMode:s,roundingPriority:s,trailingZeroDisplay:s";
}

/**
 * The %Intl% object.
 * @value globalThis.Intl
 * @attributes writable configurable
 */
function IntlObject(): Obj {
  return ObjectCreate(RealmIntrinsic("ObjectPrototype"));
}

/**
 * Intl [ %Symbol.toStringTag% ].
 * @value Intl[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlToStringTag(): Str {
  return "Intl";
}

/**
 * Intl.getCanonicalLocales ( locales ).
 * @builtin Intl.getCanonicalLocales
 */
function IntlGetCanonicalLocales(locales?: Value): Obj {
  return IntlSplit(IntlLocales(IntlLocaleTags(locales), false), "\n", 0);
}

/** SupportedLocales ( availableLocales, requestedLocales, options ). */
function IntlSupportedLocales(locales: Value): Obj {
  return IntlSplit(IntlLocales(IntlLocaleTags(locales), true), "\n", 0);
}

// ---- NumberFormat ----------------------------------------------------------------------

/** A new NumberFormat. */
function IntlNewNumberFormat(newTarget: Value, locales: Value, options: Value): Obj {
  return IntlConstruct(newTarget, "NumberFormat", 0, locales, IntlReadOptions(options, IntlNfOptions()));
}

/**
 * Intl.NumberFormat ( [ locales [ , options ] ] ).
 * @constructor Intl.NumberFormat
 * @length 0
 */
function IntlNumberFormatConstructor(locales?: Value, options?: Value): Obj {
  return IntlNewNumberFormat(new.target, locales, options);
}

/**
 * Intl.NumberFormat.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.NumberFormat.supportedLocalesOf
 * @length 1
 */
function IntlNumberFormatSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/** ToIntlMathematicalValue: a number, or a BigInt's or string's decimal digits. */
function IntlNumeric(x: Value): Value {
  const p: Value = ToPrimitive(x, "number");
  if (IsBigInt(p)) {
    return BigIntToString(p, 10);
  }
  if (IsString(p)) {
    return p;
  }
  return ToNumber(p);
}

/**
 * get Intl.NumberFormat.prototype.format.
 * @getter Intl.NumberFormat.prototype.format
 */
function IntlNumberFormatPrototypeFormat(this: Value): Value {
  IntlThis(this, 0, "get Intl.NumberFormat.prototype.format");
  return IntlBoundFunction(this, 25, 1);
}

/**
 * Intl.NumberFormat.prototype.formatToParts ( x ).
 * @builtin Intl.NumberFormat.prototype.formatToParts
 */
function IntlNumberFormatPrototypeFormatToParts(this: Value, x?: Value): Obj {
  const spec: Str = IntlThis(this, 0, "Intl.NumberFormat.prototype.formatToParts");
  return IntlParts(IntlFormat(0, spec, IntlNumeric(x), undefined, true), undefined);
}

/**
 * Intl.NumberFormat.prototype.resolvedOptions ( ).
 * @builtin Intl.NumberFormat.prototype.resolvedOptions
 */
function IntlNumberFormatPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 0, "Intl.NumberFormat.prototype.resolvedOptions"));
}

/**
 * Intl.NumberFormat.prototype [ %Symbol.toStringTag% ].
 * @value Intl.NumberFormat.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlNumberFormatToStringTag(): Str {
  return "Intl.NumberFormat";
}

// ---- DateTimeFormat --------------------------------------------------------------------

/**
 * A new DateTimeFormat; required and defaults as ToDateTimeOptions takes them (any, date
 * or time; date, time or all).
 */
function IntlNewDateTimeFormat(newTarget: Value, locales: Value, options: Value, required: Str, defaults: Str): Obj {
  let raw: Str = IntlReadOptions(options, IntlDtfOptions());
  raw = IntlWith(raw, "@required", required);
  raw = IntlWith(raw, "@defaults", defaults);
  raw = IntlWith(raw, "@offset", NumberToString(0 - DateTimezoneOffset(0), 10));
  return IntlConstruct(newTarget, "DateTimeFormat", 1, locales, raw);
}

/**
 * Intl.DateTimeFormat ( [ locales [ , options ] ] ).
 * @constructor Intl.DateTimeFormat
 * @length 0
 */
function IntlDateTimeFormatConstructor(locales?: Value, options?: Value): Obj {
  return IntlNewDateTimeFormat(new.target, locales, options, "any", "date");
}

/**
 * Intl.DateTimeFormat.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.DateTimeFormat.supportedLocalesOf
 * @length 1
 */
function IntlDateTimeFormatSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/** The time value a DateTimeFormat formats: now for undefined. */
function IntlDateValue(x: Value): f64 {
  if (x === undefined) {
    return DateNow();
  }
  return ToNumber(x);
}

/**
 * get Intl.DateTimeFormat.prototype.format.
 * @getter Intl.DateTimeFormat.prototype.format
 */
function IntlDateTimeFormatPrototypeFormat(this: Value): Value {
  IntlThis(this, 1, "get Intl.DateTimeFormat.prototype.format");
  return IntlBoundFunction(this, 25, 1);
}

/**
 * Intl.DateTimeFormat.prototype.formatToParts ( date ).
 * @builtin Intl.DateTimeFormat.prototype.formatToParts
 */
function IntlDateTimeFormatPrototypeFormatToParts(this: Value, date?: Value): Obj {
  const spec: Str = IntlThis(this, 1, "Intl.DateTimeFormat.prototype.formatToParts");
  return IntlParts(IntlFormat(1, spec, IntlDateValue(date), undefined, true), undefined);
}

/**
 * Intl.DateTimeFormat.prototype.resolvedOptions ( ).
 * @builtin Intl.DateTimeFormat.prototype.resolvedOptions
 */
function IntlDateTimeFormatPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 1, "Intl.DateTimeFormat.prototype.resolvedOptions"));
}

/**
 * Intl.DateTimeFormat.prototype [ %Symbol.toStringTag% ].
 * @value Intl.DateTimeFormat.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlDateTimeFormatToStringTag(): Str {
  return "Intl.DateTimeFormat";
}

// ---- Collator --------------------------------------------------------------------------

/** A new Collator. */
function IntlNewCollator(newTarget: Value, locales: Value, options: Value): Obj {
  return IntlConstruct(newTarget, "Collator", 2, locales, IntlReadOptions(options, IntlCollatorOptions()));
}

/**
 * Intl.Collator ( [ locales [ , options ] ] ).
 * @constructor Intl.Collator
 * @length 0
 */
function IntlCollatorConstructor(locales?: Value, options?: Value): Obj {
  return IntlNewCollator(new.target, locales, options);
}

/**
 * Intl.Collator.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.Collator.supportedLocalesOf
 * @length 1
 */
function IntlCollatorSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/**
 * get Intl.Collator.prototype.compare.
 * @getter Intl.Collator.prototype.compare
 */
function IntlCollatorPrototypeCompare(this: Value): Value {
  IntlThis(this, 2, "get Intl.Collator.prototype.compare");
  return IntlBoundFunction(this, 26, 2);
}

/**
 * Intl.Collator.prototype.resolvedOptions ( ).
 * @builtin Intl.Collator.prototype.resolvedOptions
 */
function IntlCollatorPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 2, "Intl.Collator.prototype.resolvedOptions"));
}

/**
 * Intl.Collator.prototype [ %Symbol.toStringTag% ].
 * @value Intl.Collator.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlCollatorToStringTag(): Str {
  return "Intl.Collator";
}

// ---- PluralRules -----------------------------------------------------------------------

/**
 * Intl.PluralRules ( [ locales [ , options ] ] ).
 * @constructor Intl.PluralRules
 * @length 0
 */
function IntlPluralRulesConstructor(locales?: Value, options?: Value): Obj {
  IntlRequireNew(new.target, "PluralRules");
  return IntlConstruct(new.target, "PluralRules", 3, locales, IntlReadOptions(options, IntlPluralOptions()));
}

/**
 * Intl.PluralRules.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.PluralRules.supportedLocalesOf
 * @length 1
 */
function IntlPluralRulesSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/**
 * Intl.PluralRules.prototype.select ( value ).
 * @builtin Intl.PluralRules.prototype.select
 */
function IntlPluralRulesPrototypeSelect(this: Value, value?: Value): Str {
  const spec: Str = IntlThis(this, 3, "Intl.PluralRules.prototype.select");
  return AsStr(IntlFormat(3, spec, ToNumber(value), undefined, false));
}

/**
 * Intl.PluralRules.prototype.resolvedOptions ( ).
 * @builtin Intl.PluralRules.prototype.resolvedOptions
 */
function IntlPluralRulesPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 3, "Intl.PluralRules.prototype.resolvedOptions"));
}

/**
 * Intl.PluralRules.prototype [ %Symbol.toStringTag% ].
 * @value Intl.PluralRules.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlPluralRulesToStringTag(): Str {
  return "Intl.PluralRules";
}

// ---- ListFormat ------------------------------------------------------------------------

/**
 * Intl.ListFormat ( [ locales [ , options ] ] ).
 * @constructor Intl.ListFormat
 * @length 0
 */
function IntlListFormatConstructor(locales?: Value, options?: Value): Obj {
  IntlRequireNew(new.target, "ListFormat");
  return IntlConstruct(new.target, "ListFormat", 4, locales, IntlReadOptions(options, "localeMatcher:s,type:s,style:s"));
}

/**
 * Intl.ListFormat.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.ListFormat.supportedLocalesOf
 * @length 1
 */
function IntlListFormatSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/** StringListFromIterable, joined by \u0001. */
function IntlStringList(iterable: Value): Str {
  if (iterable === undefined) {
    return "";
  }
  const list: Obj = IterableToList(iterable);
  const len: f64 = LengthOfArrayLike(list);
  let out: Str = "";
  let i: f64 = 0;
  while (i < len) {
    const v: Value = Get(list, i);
    if (!IsString(v)) {
      throw NewTypeError("Iterable yielded " + (IsSymbol(v) ? "Symbol()" : ToString(v)) + " which is not a string");
    }
    out = i === 0 ? AsStr(v) : out + "\u0001" + AsStr(v);
    i += 1;
  }
  return out;
}

/**
 * Intl.ListFormat.prototype.format ( list ).
 * @builtin Intl.ListFormat.prototype.format
 */
function IntlListFormatPrototypeFormat(this: Value, list?: Value): Str {
  const spec: Str = IntlThis(this, 4, "Intl.ListFormat.prototype.format");
  return AsStr(IntlFormat(4, spec, IntlStringList(list), undefined, false));
}

/**
 * Intl.ListFormat.prototype.formatToParts ( list ).
 * @builtin Intl.ListFormat.prototype.formatToParts
 */
function IntlListFormatPrototypeFormatToParts(this: Value, list?: Value): Obj {
  const spec: Str = IntlThis(this, 4, "Intl.ListFormat.prototype.formatToParts");
  return IntlParts(IntlFormat(4, spec, IntlStringList(list), undefined, true), undefined);
}

/**
 * Intl.ListFormat.prototype.resolvedOptions ( ).
 * @builtin Intl.ListFormat.prototype.resolvedOptions
 */
function IntlListFormatPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 4, "Intl.ListFormat.prototype.resolvedOptions"));
}

/**
 * Intl.ListFormat.prototype [ %Symbol.toStringTag% ].
 * @value Intl.ListFormat.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlListFormatToStringTag(): Str {
  return "Intl.ListFormat";
}

// ---- RelativeTimeFormat ----------------------------------------------------------------

/**
 * Intl.RelativeTimeFormat ( [ locales [ , options ] ] ).
 * @constructor Intl.RelativeTimeFormat
 * @length 0
 */
function IntlRelativeTimeFormatConstructor(locales?: Value, options?: Value): Obj {
  IntlRequireNew(new.target, "RelativeTimeFormat");
  const raw: Str = IntlReadOptions(options, "localeMatcher:s,numberingSystem:s,style:s,numeric:s");
  return IntlConstruct(new.target, "RelativeTimeFormat", 5, locales, raw);
}

/**
 * Intl.RelativeTimeFormat.supportedLocalesOf ( locales [ , options ] ).
 * @builtin Intl.RelativeTimeFormat.supportedLocalesOf
 * @length 1
 */
function IntlRelativeTimeFormatSupportedLocalesOf(locales?: Value): Obj {
  return IntlSupportedLocales(locales);
}

/**
 * Intl.RelativeTimeFormat.prototype.format ( value, unit ).
 * @builtin Intl.RelativeTimeFormat.prototype.format
 */
function IntlRelativeTimeFormatPrototypeFormat(this: Value, value?: Value, unit?: Value): Str {
  const spec: Str = IntlThis(this, 5, "Intl.RelativeTimeFormat.prototype.format");
  return AsStr(IntlFormat(5, spec, ToNumber(value), ToString(unit), false));
}

/**
 * Intl.RelativeTimeFormat.prototype.formatToParts ( value, unit ).
 * @builtin Intl.RelativeTimeFormat.prototype.formatToParts
 */
function IntlRelativeTimeFormatPrototypeFormatToParts(this: Value, value?: Value, unit?: Value): Obj {
  const spec: Str = IntlThis(this, 5, "Intl.RelativeTimeFormat.prototype.formatToParts");
  const u: Str = ToString(unit);
  const parts: Value = IntlFormat(5, spec, ToNumber(value), u, true);
  const singular: Str = StrLength(u) > 1 && StrCharCodeAt(u, StrLength(u) - 1) === 115 ? StrSubstring(u, 0, StrLength(u) - 1) : u;
  return IntlParts(parts, singular);
}

/**
 * Intl.RelativeTimeFormat.prototype.resolvedOptions ( ).
 * @builtin Intl.RelativeTimeFormat.prototype.resolvedOptions
 */
function IntlRelativeTimeFormatPrototypeResolvedOptions(this: Value): Obj {
  return IntlResolvedObject(IntlThis(this, 5, "Intl.RelativeTimeFormat.prototype.resolvedOptions"));
}

/**
 * Intl.RelativeTimeFormat.prototype [ %Symbol.toStringTag% ].
 * @value Intl.RelativeTimeFormat.prototype[Symbol.toStringTag]
 * @attributes configurable
 */
function IntlRelativeTimeFormatToStringTag(): Str {
  return "Intl.RelativeTimeFormat";
}

// ---- the locale-sensitive methods of other builtins (ECMA-402 chapters 19 to 21) -------

/** Number.prototype.toLocaleString and BigInt.prototype.toLocaleString with Intl. */
function IntlNumberToLocaleString(x: Value, locales: Value, options: Value): Str {
  const nf: Obj = IntlNewNumberFormat(undefined, locales, options);
  return AsStr(IntlFormat(0, AsStr(IntlSpec(nf, 0)), IntlNumeric(x), undefined, false));
}

/** Date.prototype.toLocale(Date|Time)String with Intl: "Invalid Date" for NaN. */
function IntlDateToLocaleString(t: f64, locales: Value, options: Value, required: Str, defaults: Str): Str {
  if (t !== t) {
    return "Invalid Date";
  }
  const dtf: Obj = IntlNewDateTimeFormat(undefined, locales, options, required, defaults);
  return AsStr(IntlFormat(1, AsStr(IntlSpec(dtf, 1)), t, undefined, false));
}

/** String.prototype.localeCompare with Intl. */
function IntlLocaleCompare(s: Str, that: Str, locales: Value, options: Value): f64 {
  const c: Obj = IntlNewCollator(undefined, locales, options);
  return AsNumber(IntlFormat(2, AsStr(IntlSpec(c, 2)), s, that, false));
}
