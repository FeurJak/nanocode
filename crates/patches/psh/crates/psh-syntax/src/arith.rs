//! Bash arithmetic: a parser for the text of `$(( ))`, `(( ))`, `for (( ;; ))`, array
//! subscripts and `let`, as bash parses it after expansion.
//!
//! The input is the expanded text (bytes); spans are byte offsets into it. Numbers are
//! decoded here (`10`, `0x1f`, `017`, `64#@_`, wrapping to 64 bits as bash does);
//! variables stay names, because bash evaluates their values as arithmetic in turn.
//! Errors carry the offset bash reports as the "error token" and render bash's message.
//!
//! Precedence, lowest first: `,` — assignments (right) — `?:` (right) — `||` — `&&` —
//! `|` — `^` — `&` — `==` `!=` — `<` `>` `<=` `>=` — `<<` `>>` — `+` `-` — `*` `/` `%`
//! — `**` (right) — unary `+` `-` `!` `~` and `++x` `--x` — `x++` `x--`.

use alloc::boxed::Box;
use alloc::string::String;

use crate::span::Span;

/// An arithmetic expression node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArithExpr {
    pub span: Span,
    pub kind: ArithKind,
    /// Start of the node's last token: bash names the text from there as the error
    /// token of a runtime error in this operand (`1/0` → `0`).
    pub last: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArithKind {
    /// A constant, decoded.
    Num(i64),
    /// A variable or array element.
    Var(LValue),
    /// `+x`, `-x`, `!x`, `~x`.
    Unary { op: UnaryOp, expr: Box<ArithExpr> },
    /// `++x`, `--x`, `x++`, `x--`.
    IncDec {
        target: LValue,
        inc: bool,
        post: bool,
    },
    Binary {
        op: BinOp,
        lhs: Box<ArithExpr>,
        rhs: Box<ArithExpr>,
    },
    /// `cond ? then : otherwise`.
    Cond {
        cond: Box<ArithExpr>,
        then: Box<ArithExpr>,
        otherwise: Box<ArithExpr>,
    },
    /// `target = value`, or with `op`: `target op= value`.
    Assign {
        target: LValue,
        op: Option<BinOp>,
        value: Box<ArithExpr>,
    },
    /// `expr`, then `error`. Bash evaluates as it parses, so what it read before a
    /// syntax error has run when the error is reported: `a/b/c.txt` divides by `b`
    /// first, and `--n++` decrements n before the lexer rejects the `++` after it.
    Fail {
        expr: Box<ArithExpr>,
        error: ArithError,
    },
}

/// Something that can be assigned: `name` or `name[subscript]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LValue {
    pub span: Span,
    pub name: Span,
    /// The subscript text between the brackets, unparsed: an arithmetic expression for
    /// indexed arrays, a key for associative ones; the variable's type decides.
    pub index: Option<Span>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    /// `!`
    Not,
    /// `~`
    BitNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    /// `**`
    Pow,
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Shl,
    Shr,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    BitAnd,
    BitXor,
    BitOr,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `,`
    Comma,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Pow => "**",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::BitAnd => "&",
            BinOp::BitXor => "^",
            BinOp::BitOr => "|",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Comma => ",",
        }
    }
}

/// What went wrong, with bash's wording (see [`ArithError::message`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithErrorKind {
    OperandExpected,
    InvalidOperator,
    /// Tokens left after a complete expression.
    SyntaxError,
    MissingParen,
    ColonExpected,
    ExpressionExpected,
    NotAVariable,
    BadSubscript,
    ValueTooGreat,
    InvalidBase,
    InvalidConstant,
    InvalidNumber,
    RecursionLevel,
    DivisionByZero,
    NegativeExponent,
    /// `++` or `--` right after `--x` or `++x`.
    IncRequiresLvalue,
    DecRequiresLvalue,
}

/// An arithmetic error. Bash prints it as `PREFIX: MESSAGE (error token is "TOKEN")`,
/// where PREFIX is the whole expression (leading blanks removed) and TOKEN the text from
/// [`ArithError::token`] to the end, or both are the number for malformed numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArithError {
    pub kind: ArithErrorKind,
    /// Where the error token starts.
    pub token: u32,
    /// For a malformed number: the text bash names, from the start of the expression
    /// to the end of the number (which starts at `token`). `None`: the whole expression.
    pub prefix: Option<Span>,
}

impl ArithError {
    pub fn new(kind: ArithErrorKind, token: u32, prefix: Option<Span>) -> ArithError {
        ArithError {
            kind,
            token,
            prefix,
        }
    }

    pub fn message(&self) -> &'static str {
        match self.kind {
            ArithErrorKind::OperandExpected => "syntax error: operand expected",
            ArithErrorKind::InvalidOperator => {
                "syntax error: invalid arithmetic operator"
            }
            ArithErrorKind::SyntaxError => "syntax error in expression",
            ArithErrorKind::MissingParen => "missing `)'",
            ArithErrorKind::ColonExpected => "`:' expected for conditional expression",
            ArithErrorKind::ExpressionExpected => "expression expected",
            ArithErrorKind::NotAVariable => "attempted assignment to non-variable",
            ArithErrorKind::BadSubscript => "bad array subscript",
            ArithErrorKind::ValueTooGreat => "value too great for base",
            ArithErrorKind::InvalidBase => "invalid arithmetic base",
            ArithErrorKind::InvalidConstant => "invalid integer constant",
            ArithErrorKind::InvalidNumber => "invalid number",
            ArithErrorKind::RecursionLevel => "expression recursion level exceeded",
            ArithErrorKind::DivisionByZero => "division by 0",
            ArithErrorKind::NegativeExponent => "exponent less than 0",
            ArithErrorKind::IncRequiresLvalue => "++: assignment requires lvalue",
            ArithErrorKind::DecRequiresLvalue => "--: assignment requires lvalue",
        }
    }

    /// Bash's message for an error in `text` (the text that was parsed), without the
    /// context prefix (`bash: line N: `, `let: `, `((: `) the caller adds. Invalid UTF-8
    /// is shown with replacement characters.
    pub fn bash_text(&self, text: &[u8]) -> String {
        let prefix: &[u8] = match self.prefix {
            Some(s) => &text[s.range()],
            None => {
                let start = text
                    .iter()
                    .position(|c| !matches!(c, b' ' | b'\t'))
                    .unwrap_or(text.len());
                &text[start..]
            }
        };
        // For a malformed number the error token is the number alone.
        let token = match self.prefix {
            Some(s) => &text[self.token as usize..s.end as usize],
            None => &text[(self.token as usize).min(text.len())..],
        };
        let mut out = String::new();
        out.push_str(&String::from_utf8_lossy(prefix));
        out.push_str(": ");
        out.push_str(self.message());
        out.push_str(" (error token is \"");
        out.push_str(&String::from_utf8_lossy(token));
        out.push_str("\")");
        out
    }
}

/// Parses an arithmetic expression; `Ok(None)` for blank text, which bash evaluates as 0.
pub fn parse_arith(text: &[u8]) -> Result<Option<ArithExpr>, ArithError> {
    let mut p = ArithParser {
        b: text,
        tok: Tok {
            kind: TK::Eof,
            start: 0,
            end: 0,
        },
        prev_start: 0,
        depth: 0,
        partial: None,
    };
    p.bump(0)?;
    if p.tok.kind == TK::Eof {
        return Ok(None);
    }
    let e = match p.comma() {
        Ok(e) => e,
        Err(error) => {
            return match p.partial.take() {
                Some(done) => Ok(Some(fail(done, error))),
                None => Err(error),
            };
        }
    };
    if p.tok.kind != TK::Eof {
        // The whole expression has run when bash finds the tokens left after it.
        let error = p.err(ArithErrorKind::SyntaxError);
        return Ok(Some(fail(e, error)));
    }
    Ok(Some(e))
}

fn fail(expr: ArithExpr, error: ArithError) -> ArithExpr {
    let (span, last) = (expr.span, expr.last);
    ArithExpr {
        kind: ArithKind::Fail {
            expr: Box::new(expr),
            error,
        },
        span,
        last,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TK {
    Eof,
    Num(i64),
    Ident {
        name_end: u32,
        index: Option<Span>,
    },
    LParen,
    RParen,
    Question,
    Colon,
    Comma,
    Inc,
    Dec,
    Not,
    BitNot,
    Bin(BinOp),
    /// `=` (None) or `op=`.
    Assign(Option<BinOp>),
    /// A char that starts no token: "operand expected" where an operand should be,
    /// "invalid arithmetic operator" elsewhere.
    Invalid,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: TK,
    start: u32,
    end: u32,
}

const MAX_DEPTH: u32 = 200;

struct ArithParser<'a> {
    b: &'a [u8],
    tok: Tok,
    /// Start of the token before `tok`: an error at the end names it.
    prev_start: u32,
    depth: u32,
    /// After an error: what bash evaluated before reaching it, in its order.
    partial: Option<ArithExpr>,
}

fn digit_value(c: u8, base: u32) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 10),
        b'A'..=b'Z' if base <= 36 => Some((c - b'A') as u32 + 10),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 36),
        b'@' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Decodes the number token `text[start..end]` (`[0-9][0-9A-Za-z@_#]*`).
fn decode_number(text: &[u8], start: usize, end: usize) -> Result<i64, ArithError> {
    let t = &text[start..end];
    // Bash names the expression up to the end of the number, and the number.
    let first = text.iter().position(|&c| !is_space(c)).unwrap_or(start);
    let span = Span::new(first as u32, end as u32);
    let fail = |kind| ArithError::new(kind, start as u32, Some(span));
    // Read left to right like bash: `0x` hex and `0` octal take no base, so a `#` there
    // is an invalid number; a decimal prefix before `#` is the base.
    let (mut base, mut digits): (u32, &[u8]) =
        if t.len() >= 2 && t[0] == b'0' && matches!(t[1], b'x' | b'X') {
            (16, &t[2..])
        } else if t[0] == b'0' {
            (8, &t[1..])
        } else {
            (10, t)
        };
    if base == 10
        && let Some(h) = t.iter().position(|&c| c == b'#')
    {
        let mut b: u32 = 0;
        for &c in &t[..h] {
            if !c.is_ascii_digit() {
                return Err(fail(ArithErrorKind::ValueTooGreat));
            }
            b = b.saturating_mul(10).saturating_add((c - b'0') as u32);
        }
        if !(2..=64).contains(&b) {
            return Err(fail(ArithErrorKind::InvalidBase));
        }
        base = b;
        digits = &t[h + 1..];
        if digits.is_empty() {
            return Err(fail(ArithErrorKind::InvalidConstant));
        }
    }
    let mut v: i64 = 0;
    for &c in digits {
        let d = match digit_value(c, base) {
            Some(d) if d < base => d,
            _ if c == b'#' => return Err(fail(ArithErrorKind::InvalidNumber)),
            _ => return Err(fail(ArithErrorKind::ValueTooGreat)),
        };
        v = v.wrapping_mul(base as i64).wrapping_add(d as i64);
    }
    Ok(v)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n')
}

impl ArithParser<'_> {
    fn at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    fn err(&self, kind: ArithErrorKind) -> ArithError {
        let kind = match kind {
            ArithErrorKind::OperandExpected | ArithErrorKind::ExpressionExpected => kind,
            _ if self.tok.kind == TK::Invalid => ArithErrorKind::InvalidOperator,
            _ => kind,
        };
        let token = if self.tok.kind == TK::Eof {
            self.prev_start
        } else {
            self.tok.start
        };
        ArithError::new(kind, token, None)
    }

    /// Reads the token starting at or after `from`.
    fn bump(&mut self, from: usize) -> Result<(), ArithError> {
        self.prev_start = self.tok.start;
        let after_operand = matches!(self.tok.kind, TK::Num(_) | TK::RParen);
        self.tok = self.lex(from)?;
        if after_operand && self.tok.kind == TK::Invalid {
            // Bash reads the token after an operand before it applies the operator
            // waiting on it, so `1/0 .x` reports `.x`, not the division.
            return Err(ArithError::new(
                ArithErrorKind::InvalidOperator,
                self.tok.start,
                None,
            ));
        }
        if let TK::Ident { .. } = self.tok.kind {
            // Bash reads one token past a name before going on, so a malformed number
            // or subscript, or a char that starts no token, right after it is reported
            // first (`x y 08`, `x y @`).
            // That token is read the same way, so the look-ahead runs on over names.
            let mut next = self.lex(self.tok.end as usize)?;
            while let TK::Ident { .. } = next.kind {
                next = self.lex(next.end as usize)?;
            }
            if next.kind == TK::Invalid {
                return Err(ArithError::new(
                    ArithErrorKind::InvalidOperator,
                    next.start,
                    None,
                ));
            }
        }
        Ok(())
    }

    fn lex(&self, from: usize) -> Result<Tok, ArithError> {
        let mut i = from;
        while self.at(i).is_some_and(is_space) {
            i += 1;
        }
        let start = i as u32;
        let Some(c) = self.at(i) else {
            return Ok(Tok {
                kind: TK::Eof,
                start,
                end: start,
            });
        };
        let two = self.at(i + 1);
        let three = self.at(i + 2);
        let (kind, len) = match c {
            b'0'..=b'9' => {
                let mut j = i;
                while self
                    .at(j)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'@' | b'_' | b'#'))
                {
                    j += 1;
                }
                (TK::Num(decode_number(self.b, i, j)?), j - i)
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let mut j = i + 1;
                while self
                    .at(j)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
                {
                    j += 1;
                }
                let name_end = j as u32;
                let mut index = None;
                if self.at(j) == Some(b'[') {
                    let mut depth = 0usize;
                    let mut k = j + 1;
                    loop {
                        match self.at(k) {
                            None => {
                                return Err(ArithError::new(
                                    ArithErrorKind::BadSubscript,
                                    start,
                                    None,
                                ));
                            }
                            Some(b'[') => depth += 1,
                            Some(b']') if depth == 0 => break,
                            Some(b']') => depth -= 1,
                            _ => {}
                        }
                        k += 1;
                    }
                    index = Some(Span::new(j as u32 + 1, k as u32));
                    j = k + 1;
                }
                (TK::Ident { name_end, index }, j - i)
            }
            b'(' => (TK::LParen, 1),
            b')' => (TK::RParen, 1),
            b'?' => (TK::Question, 1),
            b':' => (TK::Colon, 1),
            b',' => (TK::Comma, 1),
            b'~' => (TK::BitNot, 1),
            b'+' if two == Some(b'+') => (TK::Inc, 2),
            b'-' if two == Some(b'-') => (TK::Dec, 2),
            b'*' if two == Some(b'*') => (TK::Bin(BinOp::Pow), 2),
            b'<' if two == Some(b'<') && three == Some(b'=') => (TK::Assign(Some(BinOp::Shl)), 3),
            b'>' if two == Some(b'>') && three == Some(b'=') => (TK::Assign(Some(BinOp::Shr)), 3),
            b'<' if two == Some(b'<') => (TK::Bin(BinOp::Shl), 2),
            b'>' if two == Some(b'>') => (TK::Bin(BinOp::Shr), 2),
            b'<' if two == Some(b'=') => (TK::Bin(BinOp::Le), 2),
            b'>' if two == Some(b'=') => (TK::Bin(BinOp::Ge), 2),
            b'=' if two == Some(b'=') => (TK::Bin(BinOp::Eq), 2),
            b'!' if two == Some(b'=') => (TK::Bin(BinOp::Ne), 2),
            b'&' if two == Some(b'&') => (TK::Bin(BinOp::And), 2),
            b'|' if two == Some(b'|') => (TK::Bin(BinOp::Or), 2),
            b'*' | b'/' | b'%' | b'+' | b'-' | b'&' | b'^' | b'|' if two == Some(b'=') => {
                (TK::Assign(Some(simple_op(c))), 2)
            }
            b'*' | b'/' | b'%' | b'+' | b'-' | b'&' | b'^' | b'|' | b'<' | b'>' => {
                (TK::Bin(simple_op(c)), 1)
            }
            b'=' => (TK::Assign(None), 1),
            b'!' => (TK::Not, 1),
            _ => (TK::Invalid, 1),
        };
        Ok(Tok {
            kind,
            start,
            end: start + len as u32,
        })
    }

    fn next(&mut self) -> Result<(), ArithError> {
        self.bump(self.tok.end as usize)
    }

    fn enter(&mut self) -> Result<(), ArithError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err(ArithErrorKind::RecursionLevel));
        }
        Ok(())
    }

    fn node(&self, start: u32, end: u32, last: u32, kind: ArithKind) -> ArithExpr {
        ArithExpr {
            span: Span::new(start, end),
            kind,
            last,
        }
    }

    /// On an error, records `done` as evaluated before what the failed call had done.
    fn kept<T>(&mut self, r: Result<T, ArithError>, done: &ArithExpr) -> Result<T, ArithError> {
        if r.is_err() {
            let p = match self.partial.take() {
                None => done.clone(),
                Some(p) => self.binary(BinOp::Comma, done.clone(), p),
            };
            self.partial = Some(p);
        }
        r
    }

    fn zero(&self, at: u32) -> ArithExpr {
        self.node(at, at, at, ArithKind::Num(0))
    }

    /// On an error in a branch of `cond ? then : otherwise`: the condition and the
    /// branches read so far, with the failed one's partial, so a dead branch stays dead.
    fn kept_cond<T>(
        &mut self,
        r: Result<T, ArithError>,
        c: &ArithExpr,
        then: Option<&ArithExpr>,
    ) -> Result<T, ArithError> {
        if r.is_err() {
            let at = self.tok.start;
            let p = self.partial.take();
            let (then, otherwise) = match then {
                None => (p.unwrap_or_else(|| self.zero(at)), self.zero(at)),
                Some(t) => (t.clone(), p.unwrap_or_else(|| self.zero(at))),
            };
            self.partial = Some(self.node(
                c.span.start,
                at,
                at,
                ArithKind::Cond {
                    cond: Box::new(c.clone()),
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                },
            ));
        }
        r
    }

    fn binary(&self, op: BinOp, lhs: ArithExpr, rhs: ArithExpr) -> ArithExpr {
        let (start, end, last) = (lhs.span.start, rhs.span.end, rhs.last);
        self.node(
            start,
            end,
            last,
            ArithKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        )
    }

    fn comma(&mut self) -> Result<ArithExpr, ArithError> {
        let mut lhs = self.assign()?;
        while self.tok.kind == TK::Comma {
            let r = self.next();
            self.kept(r, &lhs)?;
            let r = self.assign();
            let rhs = self.kept(r, &lhs)?;
            lhs = self.binary(BinOp::Comma, lhs, rhs);
        }
        Ok(lhs)
    }

    fn assign(&mut self) -> Result<ArithExpr, ArithError> {
        self.enter()?;
        let lhs = self.cond()?;
        let r = if let TK::Assign(op) = self.tok.kind {
            let target = match lhs.kind {
                // A parenthesized name is not a variable any more: `(x) = 1` fails.
                ArithKind::Var(lv) if lv.span == lhs.span => lv,
                _ => {
                    let e = Err(self.err(ArithErrorKind::NotAVariable));
                    return self.kept(e, &lhs);
                }
            };
            // `x op= v` reads x first; `x = v` does not.
            let r = self.next();
            let r = if op.is_some() { self.kept(r, &lhs) } else { r };
            r?;
            let r = self.assign();
            let value = if op.is_some() { self.kept(r, &lhs) } else { r }?;
            Ok(self.node(
                lhs.span.start,
                value.span.end,
                value.last,
                ArithKind::Assign {
                    target,
                    op,
                    value: Box::new(value),
                },
            ))
        } else {
            Ok(lhs)
        };
        self.depth -= 1;
        r
    }

    fn cond(&mut self) -> Result<ArithExpr, ArithError> {
        let c = self.binary_level(0)?;
        if self.tok.kind != TK::Question {
            return Ok(c);
        }
        let r = self.next();
        self.kept(r, &c)?;
        if matches!(self.tok.kind, TK::Colon | TK::Eof) {
            let e = Err(self.err(ArithErrorKind::ExpressionExpected));
            return self.kept(e, &c);
        }
        let r = self.comma();
        let then = self.kept_cond(r, &c, None)?;
        if self.tok.kind != TK::Colon {
            let e = Err(self.err(ArithErrorKind::ColonExpected));
            return self.kept_cond(e, &c, Some(&then));
        }
        let r = self.next();
        self.kept_cond(r, &c, Some(&then))?;
        if self.tok.kind == TK::Eof {
            let e = Err(self.err(ArithErrorKind::ExpressionExpected));
            return self.kept_cond(e, &c, Some(&then));
        }
        let r = self.cond();
        let otherwise = self.kept_cond(r, &c, Some(&then))?;
        Ok(self.node(
            c.span.start,
            otherwise.span.end,
            otherwise.last,
            ArithKind::Cond {
                cond: Box::new(c),
                then: Box::new(then),
                otherwise: Box::new(otherwise),
            },
        ))
    }

    /// Left-associative binary operators by precedence climbing, from `||` (0) to
    /// `* / %` (9).
    fn binary_level(&mut self, min: u8) -> Result<ArithExpr, ArithError> {
        let mut lhs = self.power()?;
        loop {
            // After an operand that is not a variable, `++` is `+` and a unary `+`,
            // unless a name follows (then it is `++name`, which cannot follow an operand).
            if matches!(self.tok.kind, TK::Inc | TK::Dec) {
                let r = self.lex(self.tok.end as usize);
                if matches!(self.kept(r, &lhs)?.kind, TK::Ident { .. }) {
                    return Ok(lhs);
                }
                self.tok.kind = TK::Bin(if self.tok.kind == TK::Inc {
                    BinOp::Add
                } else {
                    BinOp::Sub
                });
                self.tok.end = self.tok.start + 1;
            }
            let TK::Bin(op) = self.tok.kind else {
                return Ok(lhs);
            };
            let level = match op {
                BinOp::Or => 0,
                BinOp::And => 1,
                BinOp::BitOr => 2,
                BinOp::BitXor => 3,
                BinOp::BitAnd => 4,
                BinOp::Eq | BinOp::Ne => 5,
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 6,
                BinOp::Shl | BinOp::Shr => 7,
                BinOp::Add | BinOp::Sub => 8,
                BinOp::Mul | BinOp::Div | BinOp::Rem => 9,
                BinOp::Pow | BinOp::Comma => return Ok(lhs),
            };
            if level < min {
                return Ok(lhs);
            }
            let r = self.next();
            self.kept(r, &lhs)?;
            let rhs = match self.binary_level(level + 1) {
                Ok(rhs) => rhs,
                // `&&` and `||` keep their short circuit: a dead operand stays dead.
                Err(e) if matches!(op, BinOp::And | BinOp::Or) => {
                    let p = match self.partial.take() {
                        Some(p) => self.binary(op, lhs, p),
                        None => lhs,
                    };
                    self.partial = Some(p);
                    return Err(e);
                }
                Err(e) => return self.kept(Err(e), &lhs),
            };
            lhs = self.binary(op, lhs, rhs);
        }
    }

    fn power(&mut self) -> Result<ArithExpr, ArithError> {
        let base = self.unary()?;
        if self.tok.kind != TK::Bin(BinOp::Pow) {
            return Ok(base);
        }
        let r = self.next();
        self.kept(r, &base)?;
        let r = self.enter();
        self.kept(r, &base)?;
        let r = self.power();
        let exp = self.kept(r, &base)?;
        self.depth -= 1;
        Ok(self.binary(BinOp::Pow, base, exp))
    }

    fn unary(&mut self) -> Result<ArithExpr, ArithError> {
        let start = self.tok.start;
        let op = match self.tok.kind {
            TK::Bin(BinOp::Add) => UnaryOp::Plus,
            TK::Bin(BinOp::Sub) => UnaryOp::Minus,
            TK::Not => UnaryOp::Not,
            TK::BitNot => UnaryOp::BitNot,
            TK::Inc | TK::Dec => {
                let inc = self.tok.kind == TK::Inc;
                // `++name` increments; otherwise `++` is two unary `+`.
                let save = (self.tok, self.prev_start);
                self.next()?;
                if let TK::Ident { name_end, index } = self.tok.kind {
                    let lv = self.lvalue(name_end, index);
                    let last = self.tok.start;
                    let r = self.next();
                    let node = self.node(
                        start,
                        lv.span.end,
                        last,
                        ArithKind::IncDec {
                            target: lv,
                            inc,
                            post: false,
                        },
                    );
                    self.kept(r, &node)?;
                    // `--x++`: bash changes x, then fails on the second operator.
                    if let TK::Inc | TK::Dec = self.tok.kind {
                        let kind = match self.tok.kind {
                            TK::Inc => ArithErrorKind::IncRequiresLvalue,
                            _ => ArithErrorKind::DecRequiresLvalue,
                        };
                        let error = ArithError::new(kind, self.tok.start, None);
                        return self.kept(Err(error), &node);
                    }
                    return Ok(node);
                }
                (self.tok, self.prev_start) = save;
                // Re-read the second char alone.
                self.tok.end = self.tok.start + 1;
                if inc { UnaryOp::Plus } else { UnaryOp::Minus }
            }
            _ => return self.postfix(),
        };
        self.next()?;
        self.enter()?;
        let expr = self.unary()?;
        self.depth -= 1;
        Ok(self.node(
            start,
            expr.span.end,
            expr.last,
            ArithKind::Unary {
                op,
                expr: Box::new(expr),
            },
        ))
    }

    fn lvalue(&self, name_end: u32, index: Option<Span>) -> LValue {
        LValue {
            span: Span::new(self.tok.start, self.tok.end),
            name: Span::new(self.tok.start, name_end),
            index,
        }
    }

    fn postfix(&mut self) -> Result<ArithExpr, ArithError> {
        let start = self.tok.start;
        match self.tok.kind {
            TK::Num(n) => {
                let end = self.tok.end;
                self.next()?;
                Ok(self.node(start, end, start, ArithKind::Num(n)))
            }
            TK::Ident { name_end, index } => {
                let lv = self.lvalue(name_end, index);
                self.next()?;
                if matches!(self.tok.kind, TK::Inc | TK::Dec) {
                    let inc = self.tok.kind == TK::Inc;
                    let (last, end) = (self.tok.start, self.tok.end);
                    let r = self.next();
                    let node = self.node(
                        start,
                        end,
                        last,
                        ArithKind::IncDec {
                            target: lv,
                            inc,
                            post: true,
                        },
                    );
                    self.kept(r, &node)?;
                    return Ok(node);
                }
                Ok(self.node(start, lv.span.end, start, ArithKind::Var(lv)))
            }
            TK::LParen => {
                self.next()?;
                self.enter()?;
                let mut inner = self.comma()?;
                self.depth -= 1;
                if self.tok.kind != TK::RParen {
                    let e = Err(self.err(ArithErrorKind::MissingParen));
                    return self.kept(e, &inner);
                }
                inner.span = Span::new(start, self.tok.end);
                inner.last = self.tok.start;
                let r = self.next();
                self.kept(r, &inner)?;
                Ok(inner)
            }
            _ => Err(self.err(ArithErrorKind::OperandExpected)),
        }
    }
}

fn simple_op(c: u8) -> BinOp {
    match c {
        b'*' => BinOp::Mul,
        b'/' => BinOp::Div,
        b'%' => BinOp::Rem,
        b'+' => BinOp::Add,
        b'-' => BinOp::Sub,
        b'&' => BinOp::BitAnd,
        b'^' => BinOp::BitXor,
        b'|' => BinOp::BitOr,
        b'<' => BinOp::Lt,
        _ => BinOp::Gt,
    }
}

/// A compact rendering for tests and debugging: fully parenthesized, numbers decoded.
pub fn arith_dump(text: &[u8], e: &ArithExpr) -> String {
    let mut out = String::new();
    dump_into(text, e, &mut out);
    out
}

fn lvalue_text(text: &[u8], lv: &LValue) -> String {
    String::from_utf8_lossy(&text[lv.span.range()]).into_owned()
}

fn dump_into(text: &[u8], e: &ArithExpr, out: &mut String) {
    use core::fmt::Write;
    match &e.kind {
        ArithKind::Num(n) => {
            let _ = write!(out, "{n}");
        }
        ArithKind::Var(lv) => out.push_str(&lvalue_text(text, lv)),
        ArithKind::Unary { op, expr } => {
            out.push('(');
            out.push(match op {
                UnaryOp::Plus => '+',
                UnaryOp::Minus => '-',
                UnaryOp::Not => '!',
                UnaryOp::BitNot => '~',
            });
            dump_into(text, expr, out);
            out.push(')');
        }
        ArithKind::IncDec { target, inc, post } => {
            let op = if *inc { "++" } else { "--" };
            out.push('(');
            if !post {
                out.push_str(op);
            }
            out.push_str(&lvalue_text(text, target));
            if *post {
                out.push_str(op);
            }
            out.push(')');
        }
        ArithKind::Binary { op, lhs, rhs } => {
            out.push('(');
            dump_into(text, lhs, out);
            out.push(' ');
            out.push_str(op.as_str());
            out.push(' ');
            dump_into(text, rhs, out);
            out.push(')');
        }
        ArithKind::Cond {
            cond,
            then,
            otherwise,
        } => {
            out.push('(');
            dump_into(text, cond, out);
            out.push_str(" ? ");
            dump_into(text, then, out);
            out.push_str(" : ");
            dump_into(text, otherwise, out);
            out.push(')');
        }
        ArithKind::Assign { target, op, value } => {
            out.push('(');
            out.push_str(&lvalue_text(text, target));
            out.push(' ');
            if let Some(op) = op {
                out.push_str(op.as_str());
            }
            out.push_str("= ");
            dump_into(text, value, out);
            out.push(')');
        }
        ArithKind::Fail { expr, .. } => dump_into(text, expr, out),
    }
}
