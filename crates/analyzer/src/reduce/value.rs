//! The byte-backed [`Value`] + PHP-semantic conversions and comparisons.
//!
//! # Design decision (Task 1): a NEW `Value`, not an extension of `PhpValue`
//!
//! `crate::concrete::PhpValue` stores strings as a lossy Rust `String`
//! (`from_utf8_lossy`). The reducer's safety contract (spec §12.2) requires a
//! **byte-backed** string (`Vec<u8>`) so that non-UTF-8 byte strings survive
//! exactly — a lossy `String` could silently change bytes and turn a real `!=`
//! into a false `==`. `PhpValue` is also consumed by existing callers
//! (`concrete::compute`, `data_provider`), so it cannot be changed in place.
//!
//! Therefore [`Value`] is a new type with `Str(Vec<u8>)`, and [`Value::from_php`]
//! converts a `PhpValue` (the cross-check oracle's output, spec §12.2) into a
//! `Value` for comparison. The lossy→byte direction is exact for the values the
//! oracle can produce (it only ever holds UTF-8 strings), so the conversion
//! preserves the cross-check's meaning.
//!
//! # PHP-semantic fidelity
//!
//! Every conversion below is gold-tested against host `php -r` output (PHP 8;
//! the modelled rules — string↔number, overflow→float, array-key coercion — are
//! identical across 8.1–8.4). Expectations are transcribed from `php -r`, never
//! guessed (spec §8). The exact snippet + output is recorded next to each test.

use crate::concrete::PhpValue;

/// A concrete PHP value with a **byte-backed** string, for the reducer.
///
/// `Float` keeps a raw `f64` (NOT `OrderedFloat`): PHP `==`/`===`/`<=>` are the
/// hand-written [`Value::php_loose_eq`] / [`Value::php_strict_eq`] /
/// [`Value::php_compare`], never the derived `PartialEq`.
///
/// The derived `PartialEq` is **structural** (`f64` bit/value compare, byte-exact
/// strings, positional arrays) and exists ONLY for test assertions and for the
/// `PartialEq` on [`super::eval::Outcome`]. It is deliberately NOT used anywhere
/// for PHP value semantics — those go through the `php_*` methods above.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    /// Ordered key→value map (insertion order preserved), PHP array.
    Arr(Vec<(ArrayKey, Value)>),
    /// An object value: a record of fields (spec §13). `class` is the runtime
    /// FQCN (byte-exact, from the construction site); `props` is the
    /// insertion-ordered field list. Increment 2 models only immutable value
    /// objects — a method that writes `$this->prop` (a mutator) or any by-ref
    /// aliasing bails before an `Object` is ever shared (driver/eval frontier).
    Object {
        class: Vec<u8>,
        props: Vec<(Vec<u8>, Value)>,
    },
    /// A first-class closure / arrow function (Inc-4 Task A).
    ///
    /// Models `function (...) use (...) {...}` and `fn (...) => expr`, with the
    /// `use(...)` / arrow auto-capture taken **by value** at creation time (a copy
    /// of each captured variable's current `Value`). By-reference capture
    /// (`use (&$x)`) and `$this`-rebinding (`Closure::bind`) are NOT representable
    /// here — the gate/eval BAIL before ever producing such a closure (frontier:
    /// impurity, fail-closed).
    ///
    /// `params` (bare parameter names) + `body` are **raw pointers into the
    /// arena** that holds the closure's source AST. This is sound ONLY because a
    /// closure is created and invoked inside the SAME `with_program` evaluation
    /// scope (one arena, which outlives the whole `exec_statements` call). A
    /// closure that escapes to a different file's arena is never invoked by us (it
    /// reaches an unmodelled boundary and bails first). The pointers are read back
    /// to typed AST refs only inside [`super::eval`] (`invoke_closure`).
    Closure(ClosureRef),
}

/// The arena-pointer payload of a [`Value::Closure`]. Type-erased so [`value`]
/// stays free of `mago_syntax` types; [`super::eval`] casts the pointers back.
///
/// `body` is `ClosureBodyPtr::Block` for `function(){...}`/`fn(){...}` (a
/// statement body) and `ClosureBodyPtr::Expr` for `fn(...) => expr` (an arrow
/// expression body). `captured` is the by-value capture environment.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosureRef {
    /// `*const FunctionLikeParameterList` — the parameter list AST node.
    pub params: *const (),
    /// The body AST node (block or expression), tagged by kind.
    pub body: ClosureBodyPtr,
    /// Captured variables (name → value), by value at creation time.
    pub captured: Vec<(Vec<u8>, Value)>,
}

/// Which AST node kind the closure body pointer refers to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClosureBodyPtr {
    /// `*const Block` — a `{ ... }` statement body (`function`/`fn` block form).
    Block(*const ()),
    /// `*const Expression` — an arrow-function `=> expr` body.
    Expr(*const ()),
}

/// A PHP array key — only `int` or (byte) `string`. Float/bool/null keys are
/// coerced by PHP at insertion time; see [`Value::to_array_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayKey {
    Int(i64),
    Str(Vec<u8>),
}

impl Value {
    /// PHP `gettype()`-style name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Arr(_) => "array",
            // A Closure is an object (`gettype()` → "object", class "Closure").
            Value::Object { .. } | Value::Closure(_) => "object",
        }
    }

    /// Convert from the concrete-evaluator's `PhpValue` (the cross-check oracle's
    /// output, spec §12.2). `PhpValue::String` is UTF-8, so `into_bytes()` is exact.
    pub fn from_php(v: PhpValue) -> Value {
        match v {
            PhpValue::Null => Value::Null,
            PhpValue::Bool(b) => Value::Bool(b),
            PhpValue::Int(i) => Value::Int(i),
            PhpValue::Float(f) => Value::Float(f),
            PhpValue::String(s) => Value::Str(s.into_bytes()),
            PhpValue::Array(m) => Value::Arr(
                m.into_iter()
                    .map(|(k, v)| {
                        let key = match k {
                            crate::concrete::ArrayKey::Int(i) => ArrayKey::Int(i),
                            crate::concrete::ArrayKey::String(s) => ArrayKey::Str(s.into_bytes()),
                        };
                        (key, Value::from_php(v))
                    })
                    .collect(),
            ),
        }
    }

    /// PHP boolean coercion (`(bool)`).
    ///
    /// Gold (`php -r 'var_dump((bool)$x);'`): `(bool)""`→false, `(bool)"0"`→false,
    /// `(bool)"0.0"`→**true** (only the exact string "0" is falsy), `(bool)0`→false,
    /// `(bool)0.0`→false, `(bool)[]`→false, `(bool)[0]`→true, `(bool)null`→false.
    pub fn to_bool(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !(s.is_empty() || s.as_slice() == b"0"),
            Value::Arr(a) => !a.is_empty(),
            // An object (incl. a Closure) is always truthy in PHP.
            Value::Object { .. } | Value::Closure(_) => true,
        }
    }

    /// PHP integer coercion (`(int)`).
    ///
    /// Gold (`php -r 'var_dump((int)$x);'`): `(int)"10abc"`→10, `(int)"abc"`→0,
    /// `(int)"08"`→8, `(int)"0x1A"`→0, `(int)"  12  "`→12 (leading ws skipped,
    /// trailing ignored), `(int)"12.9"`→12, `(int)"1e3"`→1000, `(int)".5"`→0,
    /// `(int)"+5"`→5, `(int)1.9`→1 (truncate toward zero), `(int)true`→1.
    pub fn to_int(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Bool(b) => *b as i64,
            Value::Int(i) => *i,
            Value::Float(f) => php_float_to_int(*f),
            Value::Str(s) => str_leading_numeric(s).map(|n| n.to_int()).unwrap_or(0),
            // PHP: (int) of a non-empty array is 1, empty array is 0.
            Value::Arr(a) => (!a.is_empty()) as i64,
            // (int)$object is a PHP error/warning; the eval layer (arithmetic,
            // casts) bails on an object operand before reaching here. The `1`
            // sentinel is never observed — kept only to make the match total.
            Value::Object { .. } | Value::Closure(_) => 1,
        }
    }

    /// PHP float coercion (`(float)`).
    ///
    /// Gold (`php -r 'var_dump((float)$x);'`): `(float)"10.5"`→10.5,
    /// `(float)"1e3"`→1000.0, `(float)"abc"`→0.0, `(float)"  3.5  "`→3.5,
    /// `(float)".5"`→0.5, `(float)"0x1A"`→0.0.
    pub fn to_float(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => *b as i64 as f64,
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::Str(s) => str_leading_numeric(s).map(|n| n.to_float()).unwrap_or(0.0),
            Value::Arr(a) => (!a.is_empty()) as i64 as f64,
            // (float)$object errors in PHP; arithmetic/cast bails first (see to_int).
            Value::Object { .. } | Value::Closure(_) => 1.0,
        }
    }

    /// PHP string coercion (`(string)`), byte-exact.
    ///
    /// Numbers use PHP's `precision=14` `%G` formatting (NOT serialize_precision).
    /// Gold (`php -r 'echo (string)$x;'`): `(string)1.0`→"1", `(string)1.5`→"1.5",
    /// `(string)(0.1+0.2)`→"0.3", `(string)(1/3)`→"0.33333333333333",
    /// `(string)1e20`→"1.0E+20", `(string)true`→"1", `(string)false`→"",
    /// `(string)null`→"". Arrays cannot be stringified here (caller bails).
    pub fn to_php_string(&self) -> Option<Vec<u8>> {
        Some(match self {
            Value::Null => Vec::new(),
            Value::Bool(true) => b"1".to_vec(),
            Value::Bool(false) => Vec::new(),
            Value::Int(i) => i.to_string().into_bytes(),
            Value::Float(f) => php_float_to_string(*f).into_bytes(),
            Value::Str(s) => s.clone(),
            // PHP would emit "Array" + a notice; we bail rather than model that.
            Value::Arr(_) => return None,
            // Object→string needs `__toString` (frontier §6, not modelled in v2);
            // bail by returning None so the caller (concat/cast) abstains. A
            // Closure→string is a PHP fatal — also None (caller bails).
            Value::Object { .. } | Value::Closure(_) => return None,
        })
    }

    /// PHP array-key coercion at insertion time.
    ///
    /// Gold (`php -r '$a=[]; $a[$k]=1; foreach($a as $k=>$v){echo gettype($k);}'`):
    /// int `8` → Int(8); string `"8"` → Int(8) (canonical decimal); `"08"` → Str
    /// (leading zero); `"8.0"` → Str; `"-5"` → Int(-5); `"+5"` → Str (leading +);
    /// `"1e2"` → Str; `true` → Int(1); `false` → Int(0); `null` → Str("");
    /// `1.9` → Int(1) (truncate); `""` → Str(""). Array key → None (caller bails).
    pub fn to_array_key(&self) -> Option<ArrayKey> {
        Some(match self {
            Value::Null => ArrayKey::Str(Vec::new()),
            Value::Bool(b) => ArrayKey::Int(*b as i64),
            Value::Int(i) => ArrayKey::Int(*i),
            Value::Float(f) => ArrayKey::Int(php_float_to_int(*f)),
            Value::Str(s) => match canonical_int_string(s) {
                Some(i) => ArrayKey::Int(i),
                None => ArrayKey::Str(s.clone()),
            },
            Value::Arr(_) => return None,
            // An object/closure cannot be an array key (PHP TypeError); caller bails.
            Value::Object { .. } | Value::Closure(_) => return None,
        })
    }

    /// PHP `is_numeric()` on this value's string form (only meaningful for `Str`;
    /// non-strings: int/float are numeric, others not).
    ///
    /// Gold (`php -r 'var_dump(is_numeric($s));'`): `"10"`,`"10.5"`,`"1e3"`,
    /// `"  10"`,`"10  "`,`".5"`,`"-5"`,`"+5"`,`" 10 "` → true; `"0x1A"`,`""`,
    /// `"abc"`,`"10abc"`,`"1_000"`,`"0b11"`,`"INF"`,`"NAN"` → false.
    pub fn is_numeric(&self) -> bool {
        match self {
            Value::Int(_) | Value::Float(_) => true,
            Value::Str(s) => is_numeric_string(s),
            _ => false,
        }
    }
}

// ─── Numeric-string parsing (PHP `is_numeric_string` rules) ───────────────────

/// The numeric form of a fully-numeric string: an int if it has no `.`/`e`, else
/// a float.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NumericString {
    Int(i64),
    Float(f64),
}

impl NumericString {
    fn to_int(self) -> i64 {
        match self {
            NumericString::Int(i) => i,
            NumericString::Float(f) => php_float_to_int(f),
        }
    }
    fn to_float(self) -> f64 {
        match self {
            NumericString::Int(i) => i as f64,
            NumericString::Float(f) => f,
        }
    }
}

/// PHP truncates float→int toward zero; out-of-range / non-finite → 0 (PHP 8).
fn php_float_to_int(f: f64) -> i64 {
    if !f.is_finite() {
        return 0;
    }
    let t = f.trunc();
    if t >= i64::MAX as f64 {
        // PHP wraps via modulo 2^64 for out-of-range; the common in-range path is
        // exact. For the reducer we only need correctness inside i64; out-of-range
        // float→int is an unusual corner — saturate-to-0 here would be wrong, but
        // the eval layer never reaches this with an out-of-range float without a
        // prior overflow→float bail. Keep the in-range cast exact.
        i64::MAX
    } else if t <= i64::MIN as f64 {
        i64::MIN
    } else {
        t as i64
    }
}

/// `is_numeric()` — a *full* numeric string: optional leading whitespace,
/// optional sign, integer/float/exponent, optional trailing whitespace (PHP 8).
pub(crate) fn is_numeric_string(s: &[u8]) -> bool {
    full_numeric(s).is_some()
}

/// Parse a *full* numeric string (the `is_numeric` grammar). Returns the numeric
/// value when the ENTIRE string (modulo surrounding whitespace) is numeric.
pub(crate) fn full_numeric(s: &[u8]) -> Option<NumericString> {
    // PHP allows leading AND trailing whitespace for is_numeric (PHP 8).
    let trimmed = trim_php_ws(s);
    if trimmed.is_empty() {
        return None;
    }
    parse_numeric_prefix(trimmed, true)
}

/// Parse the *leading* numeric part of a string (the `(int)`/`(float)` cast
/// behavior): consume an optional sign + digits/float/exponent, ignore the rest.
/// Leading whitespace is skipped; a non-numeric leading char yields `None`
/// (caller treats as 0/0.0).
fn str_leading_numeric(s: &[u8]) -> Option<NumericString> {
    let t = skip_leading_ws(s);
    parse_numeric_prefix(t, false)
}

fn trim_php_ws(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|c| !is_php_ws(*c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|c| !is_php_ws(*c))
        .map(|i| i + 1)
        .unwrap_or(start);
    &s[start..end]
}

fn skip_leading_ws(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|c| !is_php_ws(*c)).unwrap_or(s.len());
    &s[start..]
}

fn is_php_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// Parse `[+-]?(digits | digits.digits | .digits)([eE][+-]?digits)?` at the start
/// of `s`. When `require_full`, the whole slice must be consumed (is_numeric);
/// otherwise a numeric prefix suffices (cast). Returns `Int` when there is no
/// `.`/`e`, else `Float`.
fn parse_numeric_prefix(s: &[u8], require_full: bool) -> Option<NumericString> {
    let bytes = s;
    let mut i = 0usize;
    let n = bytes.len();

    if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let int_start = i;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let had_int_digits = i > int_start;

    let mut is_float = false;
    if i < n && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    // Need at least one digit before the optional exponent.
    let mantissa_end = i;
    let had_any_digit = had_int_digits || (is_float && mantissa_end > int_start + 1);
    if !had_any_digit {
        return None;
    }

    // Optional exponent.
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        let exp_marker = i;
        i += 1;
        if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exp_digits_start = i;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_digits_start {
            // No exponent digits → roll back to before the 'e'.
            i = exp_marker;
        } else {
            is_float = true;
        }
    }

    if require_full && i != n {
        return None;
    }

    let text = std::str::from_utf8(&bytes[..i]).ok()?;
    if is_float {
        text.parse::<f64>().ok().map(NumericString::Float)
    } else {
        match text.parse::<i64>() {
            Ok(v) => Some(NumericString::Int(v)),
            // Integer literal that overflows i64 → PHP promotes to float.
            Err(_) => text.parse::<f64>().ok().map(NumericString::Float),
        }
    }
}

/// PHP's canonical-integer-string test for array keys
/// (`_zend_handle_numeric_str_ex`): exactly `-?[0-9]+`, no leading zeros (except
/// the single `"0"`), no `-0`, no surrounding whitespace, fits in i64.
fn canonical_int_string(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = match s[0] {
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // No leading zero unless the value is exactly "0".
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    // "-0" is not canonical (would round-trip to "0").
    if neg && digits == b"0" {
        return None;
    }
    let text = std::str::from_utf8(s).ok()?;
    text.parse::<i64>().ok()
}

// ─── Float → string (PHP `(string)` cast, precision=14) ───────────────────────

/// Format a float exactly like PHP's `(string)` cast: C `%.14G` semantics with
/// PHP post-processing (uppercase `E`, always-signed exponent, no zero-padding,
/// a `.0` mantissa tail in the exponent form).
///
/// The decimal exponent is read back from Rust's `{:e}` (correctly rounded),
/// never from `log10` (which can misclassify exact powers of ten).
pub(crate) fn php_float_to_string(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    if f.is_nan() {
        return "NAN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 { "-INF".into() } else { "INF".into() };
    }

    const PRECISION: i32 = 14;
    // Decimal exponent of the value, taken from a correctly-rounded {:e}.
    let exp = {
        let e = format!("{:e}", f); // e.g. "1.2345e14" or "5e-1"
        e.split_once('e')
            .and_then(|(_, x)| x.parse::<i32>().ok())
            .unwrap_or(0)
    };

    // C `%G`: use the exponential form when exp < -4 or exp >= precision.
    if !(-4..PRECISION).contains(&exp) {
        let s = format!("{:.*E}", (PRECISION - 1) as usize, f);
        let (mant, e) = s.split_once('E').unwrap();
        let mut mant = mant.to_string();
        if mant.contains('.') {
            while mant.ends_with('0') {
                mant.pop();
            }
            if mant.ends_with('.') {
                mant.pop();
            }
        }
        if !mant.contains('.') {
            mant.push_str(".0");
        }
        let exp_num: i32 = e.parse().unwrap();
        let sign = if exp_num < 0 { '-' } else { '+' };
        format!("{}E{}{}", mant, sign, exp_num.abs())
    } else {
        let decimals = (PRECISION - 1 - exp).max(0) as usize;
        let mut s = format!("{:.*}", decimals, f);
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    }
}

// ─── PHP 8 comparison: ==, ===, <=> ───────────────────────────────────────────
//
// All rules transcribed from host `php -r` (the PHP-8 "saner numeric strings"
// semantics; see the gold tests). The float comparison is OURS — never an
// `OrderedFloat`/structural `Eq` (spec §12.2/§12.4).

impl Value {
    /// PHP `<=>` (`zend_compare`), PHP 8 semantics. Returns `Less`/`Equal`/
    /// `Greater`.
    ///
    /// Float comparison is the raw `f64` ordering (our own), not `OrderedFloat`.
    /// NaN never arises from the modelled arithmetic; if it ever did, `<`/`>`/`==`
    /// would all be false and `cmp` would report `Equal` — callers that need
    /// exact float identity must gate via the eval cross-check, not this fn.
    pub fn php_compare(&self, other: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        use Value::*;
        match (self, other) {
            // A bool on either side → compare as bools (precedence over array).
            (Bool(_), _) | (_, Bool(_)) => cmp_i64(self.to_bool() as i64, other.to_bool() as i64),
            // null vs array: null behaves as the empty array.
            (Null, Arr(x)) => cmp_i64(0, x.len() as i64),
            (Arr(x), Null) => cmp_i64(x.len() as i64, 0),
            // null on either side (non-array, non-bool) → compare as bools.
            (Null, _) | (_, Null) => cmp_i64(self.to_bool() as i64, other.to_bool() as i64),
            // number vs number.
            (Int(a), Int(b)) => cmp_i64(*a, *b),
            (Int(_) | Float(_), Int(_) | Float(_)) => cmp_f64(self.to_float(), other.to_float()),
            // number vs string: numeric string → numeric; else string compare
            // (the number is cast to its string form).
            (Int(_) | Float(_), Str(s)) => match full_numeric(s) {
                Some(n) => cmp_numeric(self, n),
                None => cmp_bytes(&self.to_php_string().unwrap_or_default(), s),
            },
            (Str(s), Int(_) | Float(_)) => match full_numeric(s) {
                Some(n) => cmp_numeric_rev(n, other),
                None => cmp_bytes(s, &other.to_php_string().unwrap_or_default()),
            },
            // string vs string: both numeric → numeric; else byte compare.
            (Str(x), Str(y)) => match (full_numeric(x), full_numeric(y)) {
                (Some(p), Some(q)) => cmp_numeric_strings(p, q),
                _ => cmp_bytes(x, y),
            },
            // array vs array: by length, then element-wise on the lhs key order.
            (Arr(x), Arr(y)) => {
                let by_len = cmp_i64(x.len() as i64, y.len() as i64);
                if by_len != Ordering::Equal {
                    by_len
                } else {
                    let mut acc = Ordering::Equal;
                    for (k, xv) in x {
                        match y.iter().find(|(yk, _)| yk == k) {
                            Some((_, yv)) => {
                                let e = xv.php_compare(yv);
                                if e != Ordering::Equal {
                                    acc = e;
                                    break;
                                }
                            }
                            // Missing key → lhs is "uncomparable-greater" in PHP.
                            None => {
                                acc = Ordering::Greater;
                                break;
                            }
                        }
                    }
                    acc
                }
            }
            // object vs object: same class → compare prop-by-prop (lhs prop order),
            // PHP `<=>` over objects (used by `==`). Different classes are
            // "uncomparable" in PHP — we report `Greater` (the `==` path only ever
            // checks for `Equal`, so a non-`Equal` verdict is a correct `!=`).
            (
                Object {
                    class: ca,
                    props: pa,
                },
                Object {
                    class: cb,
                    props: pb,
                },
            ) => {
                if ca != cb || pa.len() != pb.len() {
                    return Ordering::Greater;
                }
                let mut acc = Ordering::Equal;
                for (k, av) in pa {
                    match pb.iter().find(|(bk, _)| bk == k) {
                        Some((_, bv)) => {
                            let e = av.php_compare(bv);
                            if e != Ordering::Equal {
                                acc = e;
                                break;
                            }
                        }
                        None => {
                            acc = Ordering::Greater;
                            break;
                        }
                    }
                }
                acc
            }
            // An object vs a non-object scalar/array is uncomparable in PHP; the
            // eval layer bails on `<`/`>`/`<=>` with an object operand, and the
            // `==`/`===` paths handle objects separately, so this is never observed.
            // Report a stable non-`Equal` direction (object > everything else).
            (Object { .. }, _) => Ordering::Greater,
            (_, Object { .. }) => Ordering::Less,
            // A Closure is uncomparable; the eval layer bails on `<`/`>`/`<=>`/
            // `===` with a closure operand, so this is never observed. Report a
            // stable non-`Equal` direction.
            (Closure(_), _) => Ordering::Greater,
            (_, Closure(_)) => Ordering::Less,
            // scalar vs array → the array is greater.
            (Arr(_), _) => Ordering::Greater,
            (_, Arr(_)) => Ordering::Less,
        }
    }

    /// PHP loose equality (`==`), PHP 8 semantics.
    ///
    /// Arrays are equal when they have the same key set and loosely-equal values
    /// (order-independent — `['a'=>1,'b'=>2] == ['b'=>2,'a'=>1]` is true). Every
    /// other pair is equal iff [`Value::php_compare`] is `Equal`.
    pub fn php_loose_eq(&self, other: &Value) -> bool {
        if let (Value::Arr(x), Value::Arr(y)) = (self, other) {
            if x.len() != y.len() {
                return false;
            }
            return x.iter().all(|(k, xv)| {
                y.iter()
                    .find(|(yk, _)| yk == k)
                    .is_some_and(|(_, yv)| xv.php_loose_eq(yv))
            });
        }
        // Object `==`: same class AND same property set with loosely-equal values
        // (order-independent, like arrays). `assertEquals`/`==` IS modelable for
        // objects (structural). `assertSame`/`===` on objects is REFERENCE
        // identity — the eval layer BAILS on that (frontier §1).
        if let (
            Value::Object {
                class: ca,
                props: pa,
            },
            Value::Object {
                class: cb,
                props: pb,
            },
        ) = (self, other)
        {
            if ca != cb || pa.len() != pb.len() {
                return false;
            }
            return pa.iter().all(|(k, av)| {
                pb.iter()
                    .find(|(bk, _)| bk == k)
                    .is_some_and(|(_, bv)| av.php_loose_eq(bv))
            });
        }
        // An object/closure vs a non-matching value is never loosely equal here.
        // (Closure `==` is reference identity; the eval layer bails on it, so a
        // closure never reaches a true-producing path — false is the safe answer.)
        if matches!(self, Value::Object { .. } | Value::Closure(_))
            || matches!(other, Value::Object { .. } | Value::Closure(_))
        {
            return false;
        }
        self.php_compare(other) == std::cmp::Ordering::Equal
    }

    /// PHP strict equality (`===`): identical type AND value; arrays must match
    /// key order too. No type juggling.
    pub fn php_strict_eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            // Float `===` is bit-for-bit value equality (our own, not OrderedFloat).
            (Float(a), Float(b)) => a == b,
            (Str(a), Str(b)) => a == b,
            (Arr(a), Arr(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ak, av), (bk, bv))| ak == bk && av.php_strict_eq(bv))
            }
            _ => false, // different types are never strictly equal
        }
    }
}

/// `assertSame($expected, $actual)` ≈ `===`.
pub fn assert_same(expected: &Value, actual: &Value) -> bool {
    expected.php_strict_eq(actual)
}

/// `assertEquals($expected, $actual)` ≈ `==` (no delta).
pub fn assert_equals(expected: &Value, actual: &Value) -> bool {
    expected.php_loose_eq(actual)
}

// `assertEquals($e, $a, delta: ...)` is NOT modelled — the caller must BailOut on
// a non-zero delta (spec §12.2/§12.4): epsilon float equality is not `==`.

fn cmp_i64(a: i64, b: i64) -> std::cmp::Ordering {
    a.cmp(&b)
}

/// Raw `f64` ordering, OUR PHP-`==` float comparison (not `OrderedFloat`).
fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn cmp_bytes(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.cmp(b)
}

/// Compare a numeric `lhs` value against the numeric form `n` of a string.
fn cmp_numeric(lhs: &Value, n: NumericString) -> std::cmp::Ordering {
    match (lhs, n) {
        (Value::Int(a), NumericString::Int(b)) => cmp_i64(*a, b),
        _ => cmp_f64(lhs.to_float(), n.to_float()),
    }
}

/// Compare the numeric form `n` of a string against a numeric `rhs` value.
fn cmp_numeric_rev(n: NumericString, rhs: &Value) -> std::cmp::Ordering {
    cmp_numeric(rhs, n).reverse()
}

/// Compare two numeric strings: both-int → integer compare (exact), else float.
fn cmp_numeric_strings(p: NumericString, q: NumericString) -> std::cmp::Ordering {
    match (p, q) {
        (NumericString::Int(a), NumericString::Int(b)) => cmp_i64(a, b),
        _ => cmp_f64(p.to_float(), q.to_float()),
    }
}

// ─── Tests (gold-tested vs host `php -r`; expectations transcribed) ───────────

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &str) -> Value {
        Value::Str(b.as_bytes().to_vec())
    }

    #[test]
    fn to_bool_matches_php() {
        // php -r 'var_dump((bool)$x);'
        assert!(!s("").to_bool());
        assert!(!s("0").to_bool());
        assert!(s("0.0").to_bool()); // only exact "0" is falsy
        assert!(s("00").to_bool());
        assert!(s(" ").to_bool());
        assert!(!Value::Int(0).to_bool());
        assert!(!Value::Float(0.0).to_bool());
        assert!(Value::Int(1).to_bool());
        assert!(!Value::Null.to_bool());
        assert!(!Value::Arr(vec![]).to_bool());
        assert!(Value::Arr(vec![(ArrayKey::Int(0), Value::Int(0))]).to_bool());
    }

    #[test]
    fn to_int_matches_php() {
        // php -r 'var_dump((int)$x);'
        assert_eq!(s("10").to_int(), 10);
        assert_eq!(s("10abc").to_int(), 10);
        assert_eq!(s("abc").to_int(), 0);
        assert_eq!(s("08").to_int(), 8);
        assert_eq!(s("0x1A").to_int(), 0);
        assert_eq!(s("  12  ").to_int(), 12);
        assert_eq!(s("12.9").to_int(), 12);
        assert_eq!(s("1e3").to_int(), 1000);
        assert_eq!(s("").to_int(), 0);
        assert_eq!(s(".5").to_int(), 0);
        assert_eq!(s("-5").to_int(), -5);
        assert_eq!(s("+5").to_int(), 5);
        assert_eq!(Value::Float(1.9).to_int(), 1);
        assert_eq!(Value::Float(-1.9).to_int(), -1);
        assert_eq!(Value::Bool(true).to_int(), 1);
        assert_eq!(Value::Null.to_int(), 0);
    }

    #[test]
    fn to_float_matches_php() {
        // php -r 'var_dump((float)$x);'
        assert_eq!(s("10").to_float(), 10.0);
        assert_eq!(s("10.5").to_float(), 10.5);
        assert_eq!(s("1e3").to_float(), 1000.0);
        assert_eq!(s("abc").to_float(), 0.0);
        assert_eq!(s("  3.5  ").to_float(), 3.5);
        assert_eq!(s(".5").to_float(), 0.5);
        assert_eq!(s("0x1A").to_float(), 0.0);
    }

    #[test]
    fn to_php_string_matches_php() {
        // php -r 'echo (string)$x;'
        assert_eq!(Value::Int(42).to_php_string().unwrap(), b"42");
        assert_eq!(Value::Float(1.0).to_php_string().unwrap(), b"1");
        assert_eq!(Value::Float(1.5).to_php_string().unwrap(), b"1.5");
        assert_eq!(Value::Float(100.0).to_php_string().unwrap(), b"100");
        assert_eq!(Value::Float(0.1 + 0.2).to_php_string().unwrap(), b"0.3");
        assert_eq!(
            Value::Float(1.0 / 3.0).to_php_string().unwrap(),
            b"0.33333333333333"
        );
        assert_eq!(Value::Float(1e20).to_php_string().unwrap(), b"1.0E+20");
        assert_eq!(Value::Float(1e-7).to_php_string().unwrap(), b"1.0E-7");
        assert_eq!(Value::Float(1e15).to_php_string().unwrap(), b"1.0E+15");
        assert_eq!(Value::Float(0.0001).to_php_string().unwrap(), b"0.0001");
        assert_eq!(
            Value::Float(9223372036854775808.0).to_php_string().unwrap(),
            b"9.2233720368548E+18"
        );
        assert_eq!(Value::Bool(true).to_php_string().unwrap(), b"1");
        assert_eq!(Value::Bool(false).to_php_string().unwrap(), b"");
        assert_eq!(Value::Null.to_php_string().unwrap(), b"");
        assert!(Value::Arr(vec![]).to_php_string().is_none());
    }

    #[test]
    fn float_to_string_boundaries() {
        // php -r 'echo (string)$x;' — exact powers of ten at the %G boundary.
        assert_eq!(php_float_to_string(1e13), "10000000000000");
        assert_eq!(php_float_to_string(1e14), "1.0E+14");
        assert_eq!(php_float_to_string(1e-4), "0.0001");
        assert_eq!(php_float_to_string(1e-5), "1.0E-5");
        assert_eq!(php_float_to_string(-0.0), "-0");
    }

    #[test]
    fn array_key_coercion_matches_php() {
        // php -r '$a=[]; $a[$k]=1; foreach($a as $k=>$v){var_dump($k);}'
        assert_eq!(Value::Int(8).to_array_key(), Some(ArrayKey::Int(8)));
        assert_eq!(s("8").to_array_key(), Some(ArrayKey::Int(8)));
        assert_eq!(s("08").to_array_key(), Some(ArrayKey::Str(b"08".to_vec())));
        assert_eq!(
            s("8.0").to_array_key(),
            Some(ArrayKey::Str(b"8.0".to_vec()))
        );
        assert_eq!(s("-5").to_array_key(), Some(ArrayKey::Int(-5)));
        assert_eq!(s("+5").to_array_key(), Some(ArrayKey::Str(b"+5".to_vec())));
        assert_eq!(
            s("1e2").to_array_key(),
            Some(ArrayKey::Str(b"1e2".to_vec()))
        );
        assert_eq!(
            s("007").to_array_key(),
            Some(ArrayKey::Str(b"007".to_vec()))
        );
        assert_eq!(s("-0").to_array_key(), Some(ArrayKey::Str(b"-0".to_vec())));
        assert_eq!(s(" 8").to_array_key(), Some(ArrayKey::Str(b" 8".to_vec())));
        assert_eq!(s("0").to_array_key(), Some(ArrayKey::Int(0)));
        assert_eq!(Value::Bool(true).to_array_key(), Some(ArrayKey::Int(1)));
        assert_eq!(Value::Bool(false).to_array_key(), Some(ArrayKey::Int(0)));
        assert_eq!(Value::Null.to_array_key(), Some(ArrayKey::Str(Vec::new())));
        assert_eq!(Value::Float(1.9).to_array_key(), Some(ArrayKey::Int(1)));
        assert_eq!(s("").to_array_key(), Some(ArrayKey::Str(Vec::new())));
        // i64::MAX is canonical; +1 overflows → stays a string key.
        assert_eq!(
            s("9223372036854775807").to_array_key(),
            Some(ArrayKey::Int(i64::MAX))
        );
        assert_eq!(
            s("9223372036854775808").to_array_key(),
            Some(ArrayKey::Str(b"9223372036854775808".to_vec()))
        );
        assert!(Value::Arr(vec![]).to_array_key().is_none());
    }

    #[test]
    fn is_numeric_matches_php() {
        // php -r 'var_dump(is_numeric($s));'
        for ok in [
            "10", "10.5", "1e3", "  10", "10  ", ".5", "-5", "+5", " 10 ",
        ] {
            assert!(s(ok).is_numeric(), "{ok:?} should be numeric");
        }
        for no in ["0x1A", "", "abc", "10abc", "1_000", "0b11", "INF", "NAN"] {
            assert!(!s(no).is_numeric(), "{no:?} should NOT be numeric");
        }
        assert!(Value::Int(1).is_numeric());
        assert!(Value::Float(1.0).is_numeric());
        assert!(!Value::Null.is_numeric());
    }

    #[test]
    fn from_php_str_is_byte_exact() {
        let v = Value::from_php(PhpValue::String("héllo".to_string()));
        match v {
            Value::Str(b) => assert_eq!(b, "héllo".as_bytes()),
            _ => panic!("expected Str"),
        }
    }

    fn i(n: i64) -> Value {
        Value::Int(n)
    }
    fn arr(items: Vec<(ArrayKey, Value)>) -> Value {
        Value::Arr(items)
    }

    #[test]
    fn loose_eq_juggling_matches_php() {
        // php -r 'var_dump($a == $b);'
        assert!(s("1").php_loose_eq(&s("01"))); // numeric strings
        assert!(s("10").php_loose_eq(&s("1e1")));
        assert!(!i(0).php_loose_eq(&s("a"))); // PHP8: 0 == 'a' is FALSE
        assert!(!s("").php_loose_eq(&s("0")));
        assert!(Value::Null.php_loose_eq(&s("")));
        assert!(Value::Null.php_loose_eq(&i(0)));
        assert!(Value::Null.php_loose_eq(&Value::Bool(false)));
        assert!(!i(0).php_loose_eq(&s(""))); // 0 == '' is FALSE (string compare)
        assert!(i(0).php_loose_eq(&Value::Null)); // but 0 == null is TRUE
        assert!(s("abc").php_loose_eq(&s("abc")));
        assert!(i(1).php_loose_eq(&Value::Float(1.0)));
        assert!(i(1).php_loose_eq(&Value::Bool(true)));
        assert!(i(0).php_loose_eq(&Value::Bool(false)));
        assert!(s("1").php_loose_eq(&i(1)));
        assert!(s("1.0").php_loose_eq(&i(1)));
        assert!(s("1.0").php_loose_eq(&s("1")));
        assert!(i(100).php_loose_eq(&s("1e2")));
        assert!(s("0").php_loose_eq(&Value::Bool(false)));
        assert!(!s("abc").php_loose_eq(&i(0))); // PHP8: non-numeric string vs 0
        assert!(!i(12).php_loose_eq(&s("12abc"))); // string compare "12" vs "12abc"
        assert!(s(" 1").php_loose_eq(&s("1"))); // leading ws numeric
        assert!(s("1 ").php_loose_eq(&s("1"))); // trailing ws numeric (PHP8)
                                                // arrays, order-independent
        let a1 = arr(vec![
            (ArrayKey::Str(b"a".to_vec()), i(1)),
            (ArrayKey::Str(b"b".to_vec()), i(2)),
        ]);
        let a2 = arr(vec![
            (ArrayKey::Str(b"b".to_vec()), i(2)),
            (ArrayKey::Str(b"a".to_vec()), i(1)),
        ]);
        assert!(a1.php_loose_eq(&a2));
        assert!(arr(vec![]).php_loose_eq(&Value::Bool(false)));
        assert!(arr(vec![]).php_loose_eq(&Value::Null));
    }

    #[test]
    fn strict_eq_matches_php() {
        // php -r 'var_dump($a === $b);'
        assert!(!i(1).php_strict_eq(&Value::Float(1.0))); // 1 === 1.0 is FALSE
        assert!(i(1).php_strict_eq(&i(1)));
        assert!(!s("1").php_strict_eq(&i(1)));
        assert!(Value::Null.php_strict_eq(&Value::Null));
        assert!(!Value::Null.php_strict_eq(&Value::Bool(false)));
        assert!(Value::Float(1.0).php_strict_eq(&Value::Float(1.0)));
        // array key-order matters for ===
        let ordered = arr(vec![
            (ArrayKey::Str(b"a".to_vec()), i(1)),
            (ArrayKey::Str(b"b".to_vec()), i(2)),
        ]);
        let reordered = arr(vec![
            (ArrayKey::Str(b"b".to_vec()), i(2)),
            (ArrayKey::Str(b"a".to_vec()), i(1)),
        ]);
        assert!(!ordered.php_strict_eq(&reordered));
        assert!(ordered.php_strict_eq(&ordered.clone()));
    }

    #[test]
    fn ordering_matches_php() {
        use std::cmp::Ordering::*;
        // php -r 'echo $a <=> $b;'
        assert_eq!(i(1).php_compare(&i(2)), Less);
        assert_eq!(s("10").php_compare(&s("9")), Greater); // both numeric → numeric
        assert_eq!(i(0).php_compare(&s("a")), Less); // "0" vs "a" string compare
        assert_eq!(s("a").php_compare(&i(0)), Greater);
        assert_eq!(s("1.5").php_compare(&s("1.50")), Equal);
        assert_eq!(i(10).php_compare(&s("10")), Equal);
        assert_eq!(s("").php_compare(&i(0)), Less); // "" vs "0"
        assert_eq!(Value::Null.php_compare(&i(1)), Less);
        assert_eq!(Value::Bool(true).php_compare(&i(2)), Equal); // both true
        assert_eq!(Value::Bool(false).php_compare(&s("")), Equal);
        assert_eq!(Value::Bool(true).php_compare(&Value::Bool(false)), Greater);
        assert_eq!(s("0x1A").php_compare(&i(26)), Less); // non-numeric → "0x1A" vs "26"
        assert_eq!(s("abc").php_compare(&s("ab")), Greater);
        assert_eq!(s("10").php_compare(&s("9a")), Less); // "9a" non-numeric → string
        assert_eq!(s("10.0").php_compare(&s("10")), Equal);
        assert_eq!(s("-1").php_compare(&s("1")), Less);
        // null vs array (null = empty array)
        assert_eq!(Value::Null.php_compare(&arr(vec![])), Equal);
        assert_eq!(
            Value::Null.php_compare(&arr(vec![(ArrayKey::Int(0), i(1))])),
            Less
        );
        // scalar vs array → array greater (but bool/array → bool compare)
        assert_eq!(i(1).php_compare(&arr(vec![(ArrayKey::Int(0), i(1))])), Less);
        assert_eq!(
            arr(vec![(ArrayKey::Int(0), i(1))]).php_compare(&Value::Bool(true)),
            Equal
        );
    }

    fn obj(class: &str, props: Vec<(&str, Value)>) -> Value {
        Value::Object {
            class: class.as_bytes().to_vec(),
            props: props
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        }
    }

    #[test]
    fn object_value_basics() {
        let p = obj("Point", vec![("x", i(1)), ("y", i(2))]);
        assert_eq!(p.type_name(), "object");
        assert!(p.to_bool()); // objects are truthy
        assert!(p.to_php_string().is_none()); // no __toString route → bail
        assert!(p.to_array_key().is_none()); // not a valid array key → bail
    }

    #[test]
    fn object_loose_eq_is_structural_same_class() {
        // assertEquals/== over objects: same class + per-prop loose, order-free.
        let a = obj("Point", vec![("x", i(1)), ("y", i(2))]);
        let b = obj("Point", vec![("y", i(2)), ("x", i(1))]); // reordered props
        assert!(a.php_loose_eq(&b));
        // loose: 1 == 1.0 holds inside the prop compare.
        let c = obj("Point", vec![("x", Value::Float(1.0)), ("y", i(2))]);
        assert!(a.php_loose_eq(&c));
        // different class → not equal.
        let d = obj("Vec2", vec![("x", i(1)), ("y", i(2))]);
        assert!(!a.php_loose_eq(&d));
        // different prop value → not equal.
        let e = obj("Point", vec![("x", i(9)), ("y", i(2))]);
        assert!(!a.php_loose_eq(&e));
        // object vs non-object → never equal.
        assert!(!a.php_loose_eq(&i(1)));
    }

    #[test]
    fn object_strict_eq_stays_false_safe_direction() {
        // `===` over objects is reference identity; we keep the safe `false`
        // direction here (the eval layer BAILS on assertSame-with-object before
        // this is reached). Two structurally-equal records are NOT `===`.
        let a = obj("Point", vec![("x", i(1))]);
        let b = obj("Point", vec![("x", i(1))]);
        assert!(!a.php_strict_eq(&b));
        assert!(!a.php_strict_eq(&i(1)));
    }

    #[test]
    fn assert_intrinsics_wrap_eq() {
        // assertSame ≈ ===, assertEquals ≈ ==.
        assert!(assert_same(&i(1), &i(1)));
        assert!(!assert_same(&i(1), &Value::Float(1.0)));
        assert!(assert_equals(&i(1), &Value::Float(1.0)));
        assert!(assert_equals(&s("1"), &i(1)));
        assert!(!assert_equals(&i(0), &s("a")));
    }
}
