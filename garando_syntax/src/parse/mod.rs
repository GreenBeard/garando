//! The main parser interface

use crate::ast::{self, CrateConfig};
use crate::codemap::{CodeMap, FilePathMapping};
use crate::errors::{ColorConfig, DiagnosticBuilder, Handler};
use crate::feature_gate::UnstableFeatures;
use crate::parse::parser::Parser;
use crate::ptr::P;
use crate::str::char_at;
use crate::symbol::Symbol;
use crate::syntax_pos::{self, FileMap, Span, NO_EXPANSION};
use crate::tokenstream::{TokenStream, TokenTree};

use std::cell::RefCell;
use std::collections::HashSet;
use std::iter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str;

use log::debug;

pub type PResult<'a, T> = Result<T, DiagnosticBuilder<'a>>;

#[macro_use]
pub mod parser;

pub mod attr;
pub mod lexer;
pub mod token;

pub mod classify;
pub mod common;
pub mod obsolete;

/// Info about a parsing session.
pub struct ParseSess {
    pub span_diagnostic: Handler,
    pub unstable_features: UnstableFeatures,
    pub config: CrateConfig,
    pub missing_fragment_specifiers: RefCell<HashSet<Span>>,
    /// Used to determine and report recursive mod inclusions
    included_mod_stack: RefCell<Vec<PathBuf>>,
    code_map: Rc<CodeMap>,
}

impl ParseSess {
    pub fn new(file_path_mapping: FilePathMapping) -> Self {
        let cm = Rc::new(CodeMap::new(file_path_mapping));
        let handler = Handler::with_tty_emitter(ColorConfig::Auto, true, false, Some(cm.clone()));
        ParseSess::with_span_handler(handler, cm)
    }

    pub fn with_span_handler(handler: Handler, code_map: Rc<CodeMap>) -> ParseSess {
        ParseSess {
            span_diagnostic: handler,
            unstable_features: UnstableFeatures::from_environment(),
            config: HashSet::new(),
            missing_fragment_specifiers: RefCell::new(HashSet::new()),
            included_mod_stack: RefCell::new(vec![]),
            code_map: code_map,
        }
    }

    pub fn codemap(&self) -> &CodeMap {
        &self.code_map
    }
}

#[derive(Clone)]
pub struct Directory {
    pub path: PathBuf,
    pub ownership: DirectoryOwnership,
}

#[derive(Copy, Clone)]
pub enum DirectoryOwnership {
    Owned,
    UnownedViaBlock,
    UnownedViaMod(bool /* legacy warnings? */),
}

// a bunch of utility functions of the form parse_<thing>_from_<source>
// where <thing> includes crate, expr, item, stmt, tts, and one that
// uses a HOF to parse anything, and <source> includes file and
// source_str.

pub fn parse_crate_from_file<'a>(input: &Path, sess: &'a ParseSess) -> PResult<'a, ast::Crate> {
    let mut parser = new_parser_from_file(sess, input);
    parser.parse_crate_mod()
}

pub fn parse_crate_attrs_from_file<'a>(
    input: &Path,
    sess: &'a ParseSess,
) -> PResult<'a, Vec<ast::Attribute>> {
    let mut parser = new_parser_from_file(sess, input);
    parser.parse_inner_attributes()
}

pub fn parse_crate_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<ast::Crate> {
    new_parser_from_source_str(sess, name, source).parse_crate_mod()
}

pub fn parse_crate_attrs_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<Vec<ast::Attribute>> {
    new_parser_from_source_str(sess, name, source).parse_inner_attributes()
}

pub fn parse_expr_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<P<ast::Expr>> {
    new_parser_from_source_str(sess, name, source).parse_expr()
}

/// Parses an item.
///
/// Returns `Ok(Some(item))` when successful, `Ok(None)` when no item was found, and`Err`
/// when a syntax error occurred.
pub fn parse_item_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<Option<P<ast::Item>>> {
    new_parser_from_source_str(sess, name, source).parse_item()
}

pub fn parse_meta_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<ast::MetaItem> {
    new_parser_from_source_str(sess, name, source).parse_meta_item()
}

pub fn parse_stmt_from_source_str(
    name: String,
    source: String,
    sess: &ParseSess,
) -> PResult<Option<ast::Stmt>> {
    new_parser_from_source_str(sess, name, source).parse_stmt()
}

pub fn parse_stream_from_source_str(name: String, source: String, sess: &ParseSess) -> TokenStream {
    filemap_to_stream(sess, sess.codemap().new_filemap(name, source))
}

// Create a new parser from a source string
pub fn new_parser_from_source_str(sess: &ParseSess, name: String, source: String) -> Parser {
    let mut parser = filemap_to_parser(sess, sess.codemap().new_filemap(name, source));
    parser.recurse_into_file_modules = false;
    parser
}

/// Create a new parser, handling errors as appropriate
/// if the file doesn't exist
pub fn new_parser_from_file<'a>(sess: &'a ParseSess, path: &Path) -> Parser<'a> {
    filemap_to_parser(sess, file_to_filemap(sess, path, None))
}

/// Given a session, a crate config, a path, and a span, add
/// the file at the given path to the codemap, and return a parser.
/// On an error, use the given span as the source of the problem.
pub fn new_sub_parser_from_file<'a>(
    sess: &'a ParseSess,
    path: &Path,
    directory_ownership: DirectoryOwnership,
    module_name: Option<String>,
    sp: Span,
) -> Parser<'a> {
    let mut p = filemap_to_parser(sess, file_to_filemap(sess, path, Some(sp)));
    p.directory.ownership = directory_ownership;
    p.root_module_name = module_name;
    p
}

/// Given a filemap and config, return a parser
pub fn filemap_to_parser(sess: &ParseSess, filemap: Rc<FileMap>) -> Parser {
    let end_pos = filemap.end_pos;
    let mut parser = stream_to_parser(sess, filemap_to_stream(sess, filemap));

    if parser.token == token::Eof && parser.span == syntax_pos::DUMMY_SP {
        parser.span = Span {
            lo: end_pos,
            hi: end_pos,
            ctxt: NO_EXPANSION,
        };
    }

    parser
}

// must preserve old name for now, because quote! from the *existing*
// compiler expands into it
pub fn new_parser_from_tts(sess: &ParseSess, tts: Vec<TokenTree>) -> Parser {
    stream_to_parser(sess, tts.into_iter().collect())
}

// base abstractions

/// Given a session and a path and an optional span (for error reporting),
/// add the path to the session's codemap and return the new filemap.
fn file_to_filemap(sess: &ParseSess, path: &Path, spanopt: Option<Span>) -> Rc<FileMap> {
    match sess.codemap().load_file(path) {
        Ok(filemap) => filemap,
        Err(e) => {
            let msg = format!("couldn't read {:?}: {}", path.display(), e);
            match spanopt {
                Some(sp) => panic!(sess.span_diagnostic.span_fatal(sp, &msg)),
                None => panic!(sess.span_diagnostic.fatal(&msg)),
            }
        }
    }
}

/// Given a filemap, produce a sequence of token-trees
pub fn filemap_to_stream(sess: &ParseSess, filemap: Rc<FileMap>) -> TokenStream {
    let mut srdr = lexer::StringReader::new(sess, filemap);
    srdr.real_token();
    panictry!(srdr.parse_all_token_trees())
}

/// Given stream and the `ParseSess`, produce a parser
pub fn stream_to_parser(sess: &ParseSess, stream: TokenStream) -> Parser {
    Parser::new(sess, stream, None, true, false)
}

/// Parse a string representing a character literal into its final form.
/// Rather than just accepting/rejecting a given literal, unescapes it as
/// well. Can take any slice prefixed by a character escape. Returns the
/// character and the number of characters consumed.
pub fn char_lit(lit: &str) -> (char, isize) {
    use std::char;

    // Handle non-escaped chars first.
    if lit.as_bytes()[0] != b'\\' {
        // If the first byte isn't '\\' it might part of a multi-byte char, so
        // get the char with chars().
        let c = lit.chars().next().unwrap();
        return (c, 1);
    }

    // Handle escaped chars.
    match lit.as_bytes()[1] as char {
        '"' => ('"', 2),
        'n' => ('\n', 2),
        'r' => ('\r', 2),
        't' => ('\t', 2),
        '\\' => ('\\', 2),
        '\'' => ('\'', 2),
        '0' => ('\0', 2),
        'x' => {
            let v = u32::from_str_radix(&lit[2..4], 16).unwrap();
            let c = char::from_u32(v).unwrap();
            (c, 4)
        }
        'u' => {
            assert_eq!(lit.as_bytes()[2], b'{');
            let idx = lit.find('}').unwrap();
            let v = u32::from_str_radix(&lit[3..idx], 16).unwrap();
            let c = char::from_u32(v).unwrap();
            (c, (idx + 1) as isize)
        }
        _ => panic!("lexer should have rejected a bad character escape {}", lit),
    }
}

pub fn escape_default(s: &str) -> String {
    s.chars()
        .map(char::escape_default)
        .flat_map(|x| x)
        .collect()
}

/// Parse a string representing a string literal into its final form. Does
/// unescaping.
pub fn str_lit(lit: &str) -> String {
    debug!("parse_str_lit: given {}", escape_default(lit));
    let mut res = String::with_capacity(lit.len());

    // FIXME #8372: This could be a for-loop if it didn't borrow the iterator
    let error = |i| format!("lexer should have rejected {} at {}", lit, i);

    /// Eat everything up to a non-whitespace
    fn eat<'a>(it: &mut iter::Peekable<str::CharIndices<'a>>) {
        loop {
            match it.peek().map(|x| x.1) {
                Some(' ') | Some('\n') | Some('\r') | Some('\t') => {
                    it.next();
                }
                _ => {
                    break;
                }
            }
        }
    }

    let mut chars = lit.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                let ch = chars.peek().unwrap_or_else(|| panic!("{}", error(i))).1;

                if ch == '\n' {
                    eat(&mut chars);
                } else if ch == '\r' {
                    chars.next();
                    let ch = chars.peek().unwrap_or_else(|| panic!("{}", error(i))).1;

                    if ch != '\n' {
                        panic!("lexer accepted bare CR");
                    }
                    eat(&mut chars);
                } else {
                    // otherwise, a normal escape
                    let (c, n) = char_lit(&lit[i..]);
                    for _ in 0..n - 1 {
                        // we don't need to move past the first \
                        chars.next();
                    }
                    res.push(c);
                }
            }
            '\r' => {
                let ch = chars.peek().unwrap_or_else(|| panic!("{}", error(i))).1;

                if ch != '\n' {
                    panic!("lexer accepted bare CR");
                }
                chars.next();
                res.push('\n');
            }
            c => res.push(c),
        }
    }

    res.shrink_to_fit(); // probably not going to do anything, unless there was an escape.
    debug!("parse_str_lit: returning {}", res);
    res
}

/// Parse a string representing a raw string literal into its final form. The
/// only operation this does is convert embedded CRLF into a single LF.
pub fn raw_str_lit(lit: &str) -> String {
    debug!("raw_str_lit: given {}", escape_default(lit));
    let mut res = String::with_capacity(lit.len());

    let mut chars = lit.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if *chars.peek().unwrap() != '\n' {
                panic!("lexer accepted bare CR");
            }
            chars.next();
            res.push('\n');
        } else {
            res.push(c);
        }
    }

    res.shrink_to_fit();
    res
}

// check if `s` looks like i32 or u1234 etc.
fn looks_like_width_suffix(first_chars: &[char], s: &str) -> bool {
    s.len() > 1
        && first_chars.contains(&char_at(s, 0))
        && s[1..].chars().all(|c| '0' <= c && c <= '9')
}

macro_rules! err {
    ($opt_diag:expr, |$span:ident, $diag:ident| $($body:tt)*) => {
        match $opt_diag {
            Some(($span, $diag)) => { $($body)* }
            None => return None,
        }
    }
}

pub fn lit_token(
    lit: token::Lit,
    suf: Option<Symbol>,
    diag: Option<(Span, &Handler)>,
) -> (bool /* suffix illegal? */, Option<ast::LitKind>) {
    use crate::ast::LitKind;

    match lit {
        token::Byte(i) => (true, Some(LitKind::Byte(byte_lit(&i.as_str()).0))),
        token::Char(i) => (true, Some(LitKind::Char(char_lit(&i.as_str()).0))),

        // There are some valid suffixes for integer and float literals,
        // so all the handling is done internally.
        token::Integer(s) => (false, integer_lit(&s.as_str(), suf, diag)),
        token::Float(s) => (false, float_lit(&s.as_str(), suf, diag)),

        token::Str_(s) => {
            let s = Symbol::intern(&str_lit(&s.as_str()));
            (true, Some(LitKind::Str(s, ast::StrStyle::Cooked)))
        }
        token::StrRaw(s, n) => {
            let s = Symbol::intern(&raw_str_lit(&s.as_str()));
            (true, Some(LitKind::Str(s, ast::StrStyle::Raw(n))))
        }
        token::ByteStr(i) => (true, Some(LitKind::ByteStr(byte_str_lit(&i.as_str())))),
        token::ByteStrRaw(i, _) => (
            true,
            Some(LitKind::ByteStr(Rc::new(i.to_string().into_bytes()))),
        ),
    }
}

fn filtered_float_lit(
    data: Symbol,
    suffix: Option<Symbol>,
    diag: Option<(Span, &Handler)>,
) -> Option<ast::LitKind> {
    debug!("filtered_float_lit: {}, {:?}", data, suffix);
    let suffix = match suffix {
        Some(suffix) => suffix,
        None => return Some(ast::LitKind::FloatUnsuffixed(data)),
    };

    Some(match &*suffix.as_str() {
        "f32" => ast::LitKind::Float(data, ast::FloatTy::F32),
        "f64" => ast::LitKind::Float(data, ast::FloatTy::F64),
        suf => {
            err!(diag, |span, diag| {
                if suf.len() >= 2 && looks_like_width_suffix(&['f'], suf) {
                    // if it looks like a width, lets try to be helpful.
                    let msg = format!("invalid width `{}` for float literal", &suf[1..]);
                    diag.struct_span_err(span, &msg)
                        .help("valid widths are 32 and 64")
                        .emit()
                } else {
                    let msg = format!("invalid suffix `{}` for float literal", suf);
                    diag.struct_span_err(span, &msg)
                        .help("valid suffixes are `f32` and `f64`")
                        .emit();
                }
            });

            ast::LitKind::FloatUnsuffixed(data)
        }
    })
}
pub fn float_lit(
    s: &str,
    suffix: Option<Symbol>,
    diag: Option<(Span, &Handler)>,
) -> Option<ast::LitKind> {
    debug!("float_lit: {:?}, {:?}", s, suffix);
    // FIXME #2252: bounds checking float literals is deferred until trans
    let s = s.chars().filter(|&c| c != '_').collect::<String>();
    filtered_float_lit(Symbol::intern(&s), suffix, diag)
}

/// Parse a string representing a byte literal into its final form. Similar to `char_lit`
pub fn byte_lit(lit: &str) -> (u8, usize) {
    let err = |i| format!("lexer accepted invalid byte literal {} step {}", lit, i);

    if lit.len() == 1 {
        (lit.as_bytes()[0], 1)
    } else {
        assert_eq!(lit.as_bytes()[0], b'\\', "{}", err(0));
        let b = match lit.as_bytes()[1] {
            b'"' => b'"',
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'\\' => b'\\',
            b'\'' => b'\'',
            b'0' => b'\0',
            _ => match u64::from_str_radix(&lit[2..4], 16).ok() {
                Some(c) => {
                    if c > 0xFF {
                        panic!(err(2))
                    } else {
                        return (c as u8, 4);
                    }
                }
                None => panic!(err(3)),
            },
        };
        (b, 2)
    }
}

pub fn byte_str_lit(lit: &str) -> Rc<Vec<u8>> {
    let mut res = Vec::with_capacity(lit.len());

    // FIXME #8372: This could be a for-loop if it didn't borrow the iterator
    let error = |i| format!("lexer should have rejected {} at {}", lit, i);

    /// Eat everything up to a non-whitespace
    fn eat<I: Iterator<Item = (usize, u8)>>(it: &mut iter::Peekable<I>) {
        loop {
            match it.peek().map(|x| x.1) {
                Some(b' ') | Some(b'\n') | Some(b'\r') | Some(b'\t') => {
                    it.next();
                }
                _ => {
                    break;
                }
            }
        }
    }

    // byte string literals *must* be ASCII, but the escapes don't have to be
    let mut chars = lit.bytes().enumerate().peekable();
    loop {
        match chars.next() {
            Some((i, b'\\')) => {
                let em = error(i);
                match chars.peek().expect(&em).1 {
                    b'\n' => eat(&mut chars),
                    b'\r' => {
                        chars.next();
                        if chars.peek().expect(&em).1 != b'\n' {
                            panic!("lexer accepted bare CR");
                        }
                        eat(&mut chars);
                    }
                    _ => {
                        // otherwise, a normal escape
                        let (c, n) = byte_lit(&lit[i..]);
                        // we don't need to move past the first \
                        for _ in 0..n - 1 {
                            chars.next();
                        }
                        res.push(c);
                    }
                }
            }
            Some((i, b'\r')) => {
                let em = error(i);
                if chars.peek().expect(&em).1 != b'\n' {
                    panic!("lexer accepted bare CR");
                }
                chars.next();
                res.push(b'\n');
            }
            Some((_, c)) => res.push(c),
            None => break,
        }
    }

    Rc::new(res)
}

pub fn integer_lit(
    s: &str,
    suffix: Option<Symbol>,
    diag: Option<(Span, &Handler)>,
) -> Option<ast::LitKind> {
    // s can only be ascii, byte indexing is fine

    let s2 = s.chars().filter(|&c| c != '_').collect::<String>();
    let mut s = &s2[..];

    debug!("integer_lit: {}, {:?}", s, suffix);

    let mut base = 10;
    let orig = s;
    let mut ty = ast::LitIntType::Unsuffixed;

    if char_at(s, 0) == '0' && s.len() > 1 {
        match char_at(s, 1) {
            'x' => base = 16,
            'o' => base = 8,
            'b' => base = 2,
            _ => {}
        }
    }

    // 1f64 and 2f32 etc. are valid float literals.
    if let Some(suf) = suffix {
        if looks_like_width_suffix(&['f'], &suf.as_str()) {
            let err = match base {
                16 => Some("hexadecimal float literal is not supported"),
                8 => Some("octal float literal is not supported"),
                2 => Some("binary float literal is not supported"),
                _ => None,
            };
            if let Some(err) = err {
                err!(diag, |span, diag| diag.span_err(span, err));
            }
            return filtered_float_lit(Symbol::intern(s), Some(suf), diag);
        }
    }

    if base != 10 {
        s = &s[2..];
    }

    if let Some(suf) = suffix {
        if suf.as_str().is_empty() {
            err!(diag, |span, diag| diag
                .span_bug(span, "found empty literal suffix in Some"));
        }
        ty = match &*suf.as_str() {
            "isize" => ast::LitIntType::Signed(ast::IntTy::Is),
            "i8" => ast::LitIntType::Signed(ast::IntTy::I8),
            "i16" => ast::LitIntType::Signed(ast::IntTy::I16),
            "i32" => ast::LitIntType::Signed(ast::IntTy::I32),
            "i64" => ast::LitIntType::Signed(ast::IntTy::I64),
            "i128" => ast::LitIntType::Signed(ast::IntTy::I128),
            "usize" => ast::LitIntType::Unsigned(ast::UintTy::Us),
            "u8" => ast::LitIntType::Unsigned(ast::UintTy::U8),
            "u16" => ast::LitIntType::Unsigned(ast::UintTy::U16),
            "u32" => ast::LitIntType::Unsigned(ast::UintTy::U32),
            "u64" => ast::LitIntType::Unsigned(ast::UintTy::U64),
            "u128" => ast::LitIntType::Unsigned(ast::UintTy::U128),
            suf => {
                // i<digits> and u<digits> look like widths, so lets
                // give an error message along those lines
                err!(diag, |span, diag| {
                    if looks_like_width_suffix(&['i', 'u'], suf) {
                        let msg = format!("invalid width `{}` for integer literal", &suf[1..]);
                        diag.struct_span_err(span, &msg)
                            .help("valid widths are 8, 16, 32, 64 and 128")
                            .emit();
                    } else {
                        let msg = format!("invalid suffix `{}` for numeric literal", suf);
                        diag.struct_span_err(span, &msg)
                            .help(
                                "the suffix must be one of the integral types \
                                   (`u32`, `isize`, etc)",
                            )
                            .emit();
                    }
                });

                ty
            }
        }
    }

    debug!(
        "integer_lit: the type is {:?}, base {:?}, the new string is {:?}, the original \
           string was {:?}, the original suffix was {:?}",
        ty, base, s, orig, suffix
    );

    Some(match u128::from_str_radix(s, base) {
        Ok(r) => ast::LitKind::Int(r, ty),
        Err(_) => {
            // small bases are lexed as if they were base 10, e.g, the string
            // might be `0b10201`. This will cause the conversion above to fail,
            // but these cases have errors in the lexer: we don't want to emit
            // two errors, and we especially don't want to emit this error since
            // it isn't necessarily true.
            let already_errored = base < 10
                && s.chars()
                    .any(|c| c.to_digit(10).map_or(false, |d| d >= base));

            if !already_errored {
                err!(diag, |span, diag| diag
                    .span_err(span, "int literal is too large"));
            }
            ast::LitKind::Int(u128::from(0u64), ty)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::Abi;
    use crate::ast::{self, Ident, PatKind};
    use crate::attr::first_attr_value_str_by_name;
    use crate::codemap::Spanned;
    use crate::parse;
    use crate::parse::parser::Parser;
    use crate::print::pprust::item_to_string;
    use crate::ptr::P;
    use crate::syntax_pos::{self, BytePos, Pos, Span, NO_EXPANSION};
    use crate::tokenstream::{self, TokenTree};
    use crate::util::parser_testing::{string_to_expr, string_to_item, string_to_stmt};
    use crate::util::parser_testing::{string_to_parser, string_to_stream};
    use crate::util::ThinVec;

    // produce a syntax_pos::span
    fn sp(a: u32, b: u32) -> Span {
        Span {
            lo: BytePos(a),
            hi: BytePos(b),
            ctxt: NO_EXPANSION,
        }
    }

    fn str2seg(s: &str, lo: u32, hi: u32) -> ast::PathSegment {
        ast::PathSegment::from_ident(Ident::from_str(s), sp(lo, hi))
    }

    #[test]
    fn path_exprs_1() {
        assert!(
            string_to_expr("a".to_string())
                == P(ast::Expr {
                    id: ast::DUMMY_NODE_ID,
                    node: ast::ExprKind::Path(
                        None,
                        ast::Path {
                            span: sp(0, 1),
                            segments: vec![str2seg("a", 0, 1)],
                        }
                    ),
                    span: sp(0, 1),
                    attrs: ThinVec::new(),
                })
        )
    }

    #[test]
    fn path_exprs_2() {
        assert!(
            string_to_expr("::a::b".to_string())
                == P(ast::Expr {
                    id: ast::DUMMY_NODE_ID,
                    node: ast::ExprKind::Path(
                        None,
                        ast::Path {
                            span: sp(0, 6),
                            segments: vec![
                                ast::PathSegment::crate_root(),
                                str2seg("a", 2, 3),
                                str2seg("b", 5, 6)
                            ]
                        }
                    ),
                    span: sp(0, 6),
                    attrs: ThinVec::new(),
                })
        )
    }

    #[should_panic]
    #[test]
    fn bad_path_expr_1() {
        string_to_expr("::abc::def::return".to_string());
    }

    // check the token-tree-ization of macros
    #[test]
    fn string_to_tts_macro() {
        let tts: Vec<_> = string_to_stream("macro_rules! zip (($a)=>($a))".to_string())
            .trees()
            .collect();
        let tts: &[TokenTree] = &tts[..];

        match (tts.len(), tts.get(0), tts.get(1), tts.get(2), tts.get(3)) {
            (
                4,
                Some(&TokenTree::Token(_, token::Ident(name_macro_rules))),
                Some(&TokenTree::Token(_, token::Not)),
                Some(&TokenTree::Token(_, token::Ident(name_zip))),
                Some(&TokenTree::Delimited(_, ref macro_delimed)),
            ) if name_macro_rules.name == "macro_rules" && name_zip.name == "zip" => {
                let tts = &macro_delimed.stream().trees().collect::<Vec<_>>();
                match (tts.len(), tts.get(0), tts.get(1), tts.get(2)) {
                    (
                        3,
                        Some(&TokenTree::Delimited(_, ref first_delimed)),
                        Some(&TokenTree::Token(_, token::FatArrow)),
                        Some(&TokenTree::Delimited(_, ref second_delimed)),
                    ) if macro_delimed.delim == token::Paren => {
                        let tts = &first_delimed.stream().trees().collect::<Vec<_>>();
                        match (tts.len(), tts.get(0), tts.get(1)) {
                            (
                                2,
                                Some(&TokenTree::Token(_, token::Dollar)),
                                Some(&TokenTree::Token(_, token::Ident(ident))),
                            ) if first_delimed.delim == token::Paren && ident.name == "a" => {}
                            _ => panic!("value 3: {:?}", *first_delimed),
                        }
                        let tts = &second_delimed.stream().trees().collect::<Vec<_>>();
                        match (tts.len(), tts.get(0), tts.get(1)) {
                            (
                                2,
                                Some(&TokenTree::Token(_, token::Dollar)),
                                Some(&TokenTree::Token(_, token::Ident(ident))),
                            ) if second_delimed.delim == token::Paren && ident.name == "a" => {}
                            _ => panic!("value 4: {:?}", *second_delimed),
                        }
                    }
                    _ => panic!("value 2: {:?}", *macro_delimed),
                }
            }
            _ => panic!("value: {:?}", tts),
        }
    }

    #[test]
    fn string_to_tts_1() {
        let tts = string_to_stream("fn a (b : i32) { b; }".to_string());

        let expected = TokenStream::concat(vec![
            TokenTree::Token(sp(0, 2), token::Ident(Ident::from_str("fn"))).into(),
            TokenTree::Token(sp(3, 4), token::Ident(Ident::from_str("a"))).into(),
            TokenTree::Delimited(
                sp(5, 14),
                tokenstream::Delimited {
                    delim: token::DelimToken::Paren,
                    tts: TokenStream::concat(vec![
                        TokenTree::Token(sp(6, 7), token::Ident(Ident::from_str("b"))).into(),
                        TokenTree::Token(sp(8, 9), token::Colon).into(),
                        TokenTree::Token(sp(10, 13), token::Ident(Ident::from_str("i32"))).into(),
                    ])
                    .into(),
                },
            )
            .into(),
            TokenTree::Delimited(
                sp(15, 21),
                tokenstream::Delimited {
                    delim: token::DelimToken::Brace,
                    tts: TokenStream::concat(vec![
                        TokenTree::Token(sp(17, 18), token::Ident(Ident::from_str("b"))).into(),
                        TokenTree::Token(sp(18, 19), token::Semi).into(),
                    ])
                    .into(),
                },
            )
            .into(),
        ]);

        assert_eq!(tts, expected);
    }

    #[test]
    fn ret_expr() {
        assert!(
            string_to_expr("return d".to_string())
                == P(ast::Expr {
                    id: ast::DUMMY_NODE_ID,
                    node: ast::ExprKind::Ret(Some(P(ast::Expr {
                        id: ast::DUMMY_NODE_ID,
                        node: ast::ExprKind::Path(
                            None,
                            ast::Path {
                                span: sp(7, 8),
                                segments: vec![str2seg("d", 7, 8)],
                            }
                        ),
                        span: sp(7, 8),
                        attrs: ThinVec::new(),
                    }))),
                    span: sp(0, 8),
                    attrs: ThinVec::new(),
                })
        )
    }

    #[test]
    fn parse_stmt_1() {
        assert!(
            string_to_stmt("b;".to_string())
                == Some(ast::Stmt {
                    node: ast::StmtKind::Expr(P(ast::Expr {
                        id: ast::DUMMY_NODE_ID,
                        node: ast::ExprKind::Path(
                            None,
                            ast::Path {
                                span: sp(0, 1),
                                segments: vec![str2seg("b", 0, 1)],
                            }
                        ),
                        span: sp(0, 1),
                        attrs: ThinVec::new()
                    })),
                    id: ast::DUMMY_NODE_ID,
                    span: sp(0, 1)
                })
        )
    }

    fn parser_done(p: Parser) {
        assert_eq!(p.token.clone(), token::Eof);
    }

    #[test]
    fn parse_ident_pat() {
        let sess = ParseSess::new(FilePathMapping::empty());
        let mut parser = string_to_parser(&sess, "b".to_string());
        assert!(
            panictry!(parser.parse_pat())
                == P(ast::Pat {
                    id: ast::DUMMY_NODE_ID,
                    node: PatKind::Ident(
                        ast::BindingMode::ByValue(ast::Mutability::Immutable),
                        Spanned {
                            span: sp(0, 1),
                            node: Ident::from_str("b")
                        },
                        None
                    ),
                    span: sp(0, 1)
                })
        );
        parser_done(parser);
    }

    // check the contents of the tt manually:
    #[test]
    fn parse_fundecl() {
        // this test depends on the intern order of "fn" and "i32"
        assert_eq!(
            string_to_item("fn a (b : i32) { b; }".to_string()),
            Some(P(ast::Item {
                ident: Ident::from_str("a"),
                attrs: Vec::new(),
                id: ast::DUMMY_NODE_ID,
                node: ast::ItemKind::Fn(
                    P(ast::FnDecl {
                        inputs: vec![ast::Arg {
                            ty: P(ast::Ty {
                                id: ast::DUMMY_NODE_ID,
                                node: ast::TyKind::Path(
                                    None,
                                    ast::Path {
                                        span: sp(10, 13),
                                        segments: vec![str2seg("i32", 10, 13)],
                                    }
                                ),
                                span: sp(10, 13)
                            }),
                            pat: P(ast::Pat {
                                id: ast::DUMMY_NODE_ID,
                                node: PatKind::Ident(
                                    ast::BindingMode::ByValue(ast::Mutability::Immutable),
                                    Spanned {
                                        span: sp(6, 7),
                                        node: Ident::from_str("b")
                                    },
                                    None
                                ),
                                span: sp(6, 7)
                            }),
                            id: ast::DUMMY_NODE_ID
                        }],
                        output: ast::FunctionRetTy::Default(sp(15, 15)),
                        variadic: false
                    }),
                    ast::Unsafety::Normal,
                    Spanned {
                        span: sp(0, 2),
                        node: ast::Constness::NotConst,
                    },
                    Abi::Rust,
                    ast::Generics {
                        // no idea on either of these:
                        lifetimes: Vec::new(),
                        ty_params: Vec::new(),
                        where_clause: ast::WhereClause {
                            id: ast::DUMMY_NODE_ID,
                            predicates: Vec::new(),
                        },
                        span: syntax_pos::DUMMY_SP,
                    },
                    P(ast::Block {
                        stmts: vec![ast::Stmt {
                            node: ast::StmtKind::Semi(P(ast::Expr {
                                id: ast::DUMMY_NODE_ID,
                                node: ast::ExprKind::Path(
                                    None,
                                    ast::Path {
                                        span: sp(17, 18),
                                        segments: vec![str2seg("b", 17, 18)],
                                    }
                                ),
                                span: sp(17, 18),
                                attrs: ThinVec::new()
                            })),
                            id: ast::DUMMY_NODE_ID,
                            span: sp(17, 19)
                        }],
                        id: ast::DUMMY_NODE_ID,
                        rules: ast::BlockCheckMode::Default, // no idea
                        span: sp(15, 21),
                    })
                ),
                vis: ast::Visibility::Inherited,
                span: sp(0, 21)
            }))
        );
    }

    #[test]
    fn parse_use() {
        let use_s = "use foo::bar::baz;";
        let vitem = string_to_item(use_s.to_string()).unwrap();
        let vitem_s = item_to_string(&vitem);
        assert_eq!(&vitem_s[..], use_s);

        let use_s = "use foo::bar as baz;";
        let vitem = string_to_item(use_s.to_string()).unwrap();
        let vitem_s = item_to_string(&vitem);
        assert_eq!(&vitem_s[..], use_s);
    }

    #[test]
    fn parse_extern_crate() {
        let ex_s = "extern crate foo;";
        let vitem = string_to_item(ex_s.to_string()).unwrap();
        let vitem_s = item_to_string(&vitem);
        assert_eq!(&vitem_s[..], ex_s);

        let ex_s = "extern crate foo as bar;";
        let vitem = string_to_item(ex_s.to_string()).unwrap();
        let vitem_s = item_to_string(&vitem);
        assert_eq!(&vitem_s[..], ex_s);
    }

    fn get_spans_of_pat_idents(src: &str) -> Vec<Span> {
        let item = string_to_item(src.to_string()).unwrap();

        struct PatIdentVisitor {
            spans: Vec<Span>,
        }
        impl<'a> crate::visit::Visitor<'a> for PatIdentVisitor {
            fn visit_pat(&mut self, p: &'a ast::Pat) {
                match p.node {
                    PatKind::Ident(_, ref spannedident, _) => {
                        self.spans.push(spannedident.span.clone());
                    }
                    _ => {
                        crate::visit::walk_pat(self, p);
                    }
                }
            }
        }
        let mut v = PatIdentVisitor { spans: Vec::new() };
        crate::visit::walk_item(&mut v, &item);
        return v.spans;
    }

    #[test]
    fn span_of_self_arg_pat_idents_are_correct() {
        let srcs = [
            "impl z { fn a (&self, &myarg: i32) {} }",
            "impl z { fn a (&mut self, &myarg: i32) {} }",
            "impl z { fn a (&'a self, &myarg: i32) {} }",
            "impl z { fn a (self, &myarg: i32) {} }",
            "impl z { fn a (self: Foo, &myarg: i32) {} }",
        ];

        for &src in &srcs {
            let spans = get_spans_of_pat_idents(src);
            let Span { lo, hi, .. } = spans[0];
            assert!(
                "self" == &src[lo.to_usize()..hi.to_usize()],
                "\"{}\" != \"self\". src=\"{}\"",
                &src[lo.to_usize()..hi.to_usize()],
                src
            )
        }
    }

    #[test]
    fn parse_exprs() {
        // just make sure that they parse....
        string_to_expr("3 + 4".to_string());
        string_to_expr("a::z.froob(b,&(987+3))".to_string());
    }

    #[test]
    fn attrs_fix_bug() {
        string_to_item(
            "pub fn mk_file_writer(path: &Path, flags: &[FileFlag])
                   -> Result<Box<Writer>, String> {
    #[cfg(windows)]
    fn wb() -> c_int {
      (O_WRONLY | libc::consts::os::extra::O_BINARY) as c_int
    }

    #[cfg(unix)]
    fn wb() -> c_int { O_WRONLY as c_int }

    let mut fflags: c_int = wb();
}"
            .to_string(),
        );
    }

    #[test]
    fn crlf_doc_comments() {
        let sess = ParseSess::new(FilePathMapping::empty());

        let name = "<source>".to_string();
        let source = "/// doc comment\r\nfn foo() {}".to_string();
        let item = parse_item_from_source_str(name.clone(), source, &sess)
            .unwrap()
            .unwrap();
        let doc = first_attr_value_str_by_name(&item.attrs, "doc").unwrap();
        assert_eq!(doc, "/// doc comment");

        let source = "/// doc comment\r\n/// line 2\r\nfn foo() {}".to_string();
        let item = parse_item_from_source_str(name.clone(), source, &sess)
            .unwrap()
            .unwrap();
        let docs = item
            .attrs
            .iter()
            .filter(|a| a.path == "doc")
            .map(|a| a.value_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let b: &[_] = &["/// doc comment".to_string(), "/// line 2".to_string()];
        assert_eq!(&docs[..], b);

        let source = "/** doc comment\r\n *  with CRLF */\r\nfn foo() {}".to_string();
        let item = parse_item_from_source_str(name, source, &sess)
            .unwrap()
            .unwrap();
        let doc = first_attr_value_str_by_name(&item.attrs, "doc").unwrap();
        assert_eq!(doc, "/** doc comment\n *  with CRLF */");
    }

    #[test]
    fn ttdelim_span() {
        let sess = ParseSess::new(FilePathMapping::empty());
        let expr = parse::parse_expr_from_source_str(
            "foo".to_string(),
            "foo!( fn main() { body } )".to_string(),
            &sess,
        )
        .unwrap();

        let tts: Vec<_> = match expr.node {
            ast::ExprKind::Mac(ref mac) => mac.node.stream().trees().collect(),
            _ => panic!("not a macro"),
        };

        let span = tts.iter().rev().next().unwrap().span();

        match sess.codemap().span_to_snippet(span) {
            Ok(s) => assert_eq!(&s[..], "{ body }"),
            Err(_) => panic!("could not get snippet"),
        }
    }

    // This tests that when parsing a string (rather than a file) we don't try
    // and read in a file for a module declaration and just parse a stub.
    // See `recurse_into_file_modules` in the parser.
    #[test]
    fn out_of_line_mod() {
        let sess = ParseSess::new(FilePathMapping::empty());
        let item = parse_item_from_source_str(
            "foo".to_owned(),
            "mod foo { struct S; mod this_does_not_exist; }".to_owned(),
            &sess,
        )
        .unwrap()
        .unwrap();

        if let ast::ItemKind::Mod(ref m) = item.node {
            assert!(m.items.len() == 2);
        } else {
            panic!();
        }
    }
}
