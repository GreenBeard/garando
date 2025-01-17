//! The source positions and related helper functions
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

#![deny(warnings)]

use std::cell::{Cell, RefCell};
use std::cmp;
use std::ops::{Add, Sub};
use std::rc::Rc;

use std::fmt;

use serde::de::{self, Deserializer, SeqAccess, Unexpected, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

pub mod hygiene;
pub use crate::hygiene::{ExpnFormat, ExpnInfo, NameAndSpan, SyntaxContext};

pub mod symbol;

pub type FileName = String;

/// Spans represent a region of code, used for error reporting. Positions in spans
/// are *absolute* positions from the beginning of the codemap, not positions
/// relative to FileMaps. Methods on the CodeMap can be used to relate spans back
/// to the original source.
/// You must be careful if the span crosses more than one file - you will not be
/// able to use many of the functions on spans in codemap and you cannot assume
/// that the length of the span = hi - lo; there may be space in the BytePos
/// range between files.
#[derive(Clone, Copy, Hash, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Span {
    pub lo: BytePos,
    pub hi: BytePos,
    /// Information about where the macro came from, if this piece of
    /// code was created by a macro expansion.
    #[serde(skip)]
    pub ctxt: SyntaxContext,
}

/// A collection of spans. Spans have two orthogonal attributes:
///
/// - they can be *primary spans*. In this case they are the locus of
///   the error, and would be rendered with `^^^`.
/// - they can have a *label*. In this case, the label is written next
///   to the mark in the snippet when we render.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiSpan {
    primary_spans: Vec<Span>,
    span_labels: Vec<(Span, String)>,
}

impl Span {
    /// Returns a new span representing just the end-point of this span
    pub fn end_point(self) -> Span {
        let lo = cmp::max(self.hi.0 - 1, self.lo.0);
        Span {
            lo: BytePos(lo),
            ..self
        }
    }

    /// Returns a new span representing the next character after the end-point of this span
    pub fn next_point(self) -> Span {
        let lo = cmp::max(self.hi.0, self.lo.0 + 1);
        Span {
            lo: BytePos(lo),
            hi: BytePos(lo),
            ..self
        }
    }

    /// Returns `self` if `self` is not the dummy span, and `other` otherwise.
    pub fn substitute_dummy(self, other: Span) -> Span {
        if self.source_equal(&DUMMY_SP) {
            other
        } else {
            self
        }
    }

    pub fn contains(self, other: Span) -> bool {
        self.lo <= other.lo && other.hi <= self.hi
    }

    /// Return true if the spans are equal with regards to the source text.
    ///
    /// Use this instead of `==` when either span could be generated code,
    /// and you only care that they point to the same bytes of source text.
    pub fn source_equal(&self, other: &Span) -> bool {
        self.lo == other.lo && self.hi == other.hi
    }

    /// Returns `Some(span)`, where the start is trimmed by the end of `other`
    pub fn trim_start(self, other: Span) -> Option<Span> {
        if self.hi > other.hi {
            Some(Span {
                lo: cmp::max(self.lo, other.hi),
                ..self
            })
        } else {
            None
        }
    }

    /// Return the source span - this is either the supplied span, or the span for
    /// the macro callsite that expanded to it.
    pub fn source_callsite(self) -> Span {
        self.ctxt
            .outer()
            .expn_info()
            .map(|info| info.call_site.source_callsite())
            .unwrap_or(self)
    }

    /// Return the source callee.
    ///
    /// Returns None if the supplied span has no expansion trace,
    /// else returns the NameAndSpan for the macro definition
    /// corresponding to the source callsite.
    pub fn source_callee(self) -> Option<NameAndSpan> {
        fn source_callee(info: ExpnInfo) -> NameAndSpan {
            match info.call_site.ctxt.outer().expn_info() {
                Some(info) => source_callee(info),
                None => info.callee,
            }
        }
        self.ctxt.outer().expn_info().map(source_callee)
    }

    /// Check if a span is "internal" to a macro in which #[unstable]
    /// items can be used (that is, a macro marked with
    /// `#[allow_internal_unstable]`).
    pub fn allows_unstable(&self) -> bool {
        match self.ctxt.outer().expn_info() {
            Some(info) => info.callee.allow_internal_unstable,
            None => false,
        }
    }

    pub fn macro_backtrace(mut self) -> Vec<MacroBacktrace> {
        let mut prev_span = DUMMY_SP;
        let mut result = vec![];
        while let Some(info) = self.ctxt.outer().expn_info() {
            let (pre, post) = match info.callee.format {
                ExpnFormat::MacroAttribute(..) => ("#[", "]"),
                ExpnFormat::MacroBang(..) => ("", "!"),
                ExpnFormat::CompilerDesugaring(..) => ("desugaring of `", "`"),
            };
            let macro_decl_name = format!("{}{}{}", pre, info.callee.name(), post);
            let def_site_span = info.callee.span;

            // Don't print recursive invocations
            if !info.call_site.source_equal(&prev_span) {
                result.push(MacroBacktrace {
                    call_site: info.call_site,
                    macro_decl_name,
                    def_site_span,
                });
            }

            prev_span = self;
            self = info.call_site;
        }
        result
    }

    pub fn to(self, end: Span) -> Span {
        // FIXME(jseyfried): self.ctxt should always equal end.ctxt here (c.f. issue #23480)
        if end.ctxt == SyntaxContext::empty() {
            Span { lo: self.lo, ..end }
        } else {
            Span { hi: end.hi, ..self }
        }
    }

    pub fn between(self, end: Span) -> Span {
        Span {
            lo: self.hi,
            hi: end.lo,
            ctxt: if end.ctxt == SyntaxContext::empty() {
                end.ctxt
            } else {
                self.ctxt
            },
        }
    }

    pub fn until(self, end: Span) -> Span {
        Span {
            lo: self.lo,
            hi: end.lo,
            ctxt: if end.ctxt == SyntaxContext::empty() {
                end.ctxt
            } else {
                self.ctxt
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpanLabel {
    /// The span we are going to include in the final snippet.
    pub span: Span,

    /// Is this a primary span? This is the "locus" of the message,
    /// and is indicated with a `^^^^` underline, versus `----`.
    pub is_primary: bool,

    /// What label should we attach to this span (if any)?
    pub label: Option<String>,
}

fn default_span_debug(span: Span, f: &mut fmt::Formatter) -> fmt::Result {
    write!(
        f,
        "Span {{ lo: {:?}, hi: {:?}, ctxt: {:?} }}",
        span.lo, span.hi, span.ctxt
    )
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        SPAN_DEBUG.with(|span_debug| span_debug.get()(*self, f))
    }
}

pub const DUMMY_SP: Span = Span {
    lo: BytePos(0),
    hi: BytePos(0),
    ctxt: NO_EXPANSION,
};

impl MultiSpan {
    pub fn from_span(primary_span: Span) -> MultiSpan {
        MultiSpan {
            primary_spans: vec![primary_span],
            span_labels: vec![],
        }
    }

    pub fn from_spans(vec: Vec<Span>) -> MultiSpan {
        MultiSpan {
            primary_spans: vec,
            span_labels: vec![],
        }
    }

    pub fn push_span_label(&mut self, span: Span, label: String) {
        self.span_labels.push((span, label));
    }

    /// Selects the first primary span (if any)
    pub fn primary_span(&self) -> Option<Span> {
        self.primary_spans.first().cloned()
    }

    /// Returns all primary spans.
    pub fn primary_spans(&self) -> &[Span] {
        &self.primary_spans
    }

    /// Replaces all occurances of one Span with another. Used to move Spans in areas that don't
    /// display well (like std macros). Returns true if replacements occurred.
    pub fn replace(&mut self, before: Span, after: Span) -> bool {
        let mut replacements_occurred = false;
        for primary_span in &mut self.primary_spans {
            if *primary_span == before {
                *primary_span = after;
                replacements_occurred = true;
            }
        }
        for span_label in &mut self.span_labels {
            if span_label.0 == before {
                span_label.0 = after;
                replacements_occurred = true;
            }
        }
        replacements_occurred
    }

    /// Returns the strings to highlight. We always ensure that there
    /// is an entry for each of the primary spans -- for each primary
    /// span P, if there is at least one label with span P, we return
    /// those labels (marked as primary). But otherwise we return
    /// `SpanLabel` instances with empty labels.
    pub fn span_labels(&self) -> Vec<SpanLabel> {
        let is_primary = |span| self.primary_spans.contains(&span);
        let mut span_labels = vec![];

        for &(span, ref label) in &self.span_labels {
            span_labels.push(SpanLabel {
                span,
                is_primary: is_primary(span),
                label: Some(label.clone()),
            });
        }

        for &span in &self.primary_spans {
            if !span_labels.iter().any(|sl| sl.span == span) {
                span_labels.push(SpanLabel {
                    span,
                    is_primary: true,
                    label: None,
                });
            }
        }

        span_labels
    }
}

impl From<Span> for MultiSpan {
    fn from(span: Span) -> MultiSpan {
        MultiSpan::from_span(span)
    }
}

pub const NO_EXPANSION: SyntaxContext = crate::hygiene::NO_EXPANSION;

/// Identifies an offset of a multi-byte character in a FileMap
#[derive(Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct MultiByteChar {
    /// The absolute offset of the character in the CodeMap
    pub pos: BytePos,
    /// The number of bytes, >=2
    pub bytes: usize,
}

/// A single source in the CodeMap.
#[derive(Clone, Serialize, Deserialize)]
pub struct FileMap {
    /// The name of the file that the source came from, source that doesn't
    /// originate from files has names between angle brackets by convention,
    /// e.g. `<anon>`
    pub name: FileName,
    /// True if the `name` field above has been modified by -Zremap-path-prefix
    pub name_was_remapped: bool,
    /// Indicates which crate this FileMap was imported from.
    #[serde(skip, default = "invalid_crate")]
    pub crate_of_origin: u32,
    /// The complete source code
    #[serde(skip)]
    pub src: Option<Rc<String>>,
    /// The start position of this source in the CodeMap
    pub start_pos: BytePos,
    /// The end position of this source in the CodeMap
    pub end_pos: BytePos,
    /// Locations of lines beginnings in the source code
    #[serde(
        serialize_with = "serialize_lines",
        deserialize_with = "deserialize_lines"
    )]
    pub lines: RefCell<Vec<BytePos>>,
    /// Locations of multi-byte characters in the source code
    pub multibyte_chars: RefCell<Vec<MultiByteChar>>,
}

fn invalid_crate() -> u32 {
    // `crate_of_origin` has to be set by the importer.
    // This value matches up with rustc::hir::def_id::INVALID_CRATE.
    // That constant is not available here unfortunately :(
    ::std::u32::MAX - 1
}

fn serialize_lines<S>(lines: &RefCell<Vec<BytePos>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let lines = lines.borrow();

    if lines.is_empty() {
        serializer.serialize_seq(Some(0))?.end()
    } else {
        let mut seq = serializer.serialize_seq(Some(lines.len() + 1))?;

        // In order to preserve some space, we exploit the fact that
        // the lines list is sorted and individual lines are
        // probably not that long. Because of that we can store lines
        // as a difference list, using as little space as possible
        // for the differences.
        let max_line_length = if lines.len() == 1 {
            0
        } else {
            lines
                .windows(2)
                .map(|w| w[1] - w[0])
                .map(|bp| bp.to_usize())
                .max()
                .unwrap()
        };

        let bytes_per_diff: u8 = match max_line_length {
            0..=0xFF => 1,
            0x100..=0xFFFF => 2,
            _ => 4,
        };

        // Encode the number of bytes used per diff.
        seq.serialize_element(&bytes_per_diff)?;

        // Encode the first element.
        seq.serialize_element(&lines[0])?;

        let diff_iter = (&lines[..]).windows(2).map(|w| (w[1] - w[0]));

        match bytes_per_diff {
            1 => {
                for diff in diff_iter {
                    seq.serialize_element(&(diff.0 as u8))?
                }
            }
            2 => {
                for diff in diff_iter {
                    seq.serialize_element(&(diff.0 as u16))?
                }
            }
            4 => {
                for diff in diff_iter {
                    seq.serialize_element(&diff.0)?
                }
            }
            _ => unreachable!(),
        }

        seq.end()
    }
}

fn deserialize_lines<'de, D>(deserializer: D) -> Result<RefCell<Vec<BytePos>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct LinesVisitor;

    impl<'de> Visitor<'de> for LinesVisitor {
        type Value = RefCell<Vec<BytePos>>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("compressed locations of lines beginnings in the source code")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut lines = Vec::with_capacity(seq.size_hint().unwrap_or(0));

            // Read the number of bytes used per diff.
            if let Some(bytes_per_diff) = seq.next_element::<u8>()? {
                // Read the first element.
                let mut line_start: BytePos = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                lines.push(line_start);

                while let Some(diff) = match bytes_per_diff {
                    1 => seq.next_element::<u8>()?.map(|u| u as u32),
                    2 => seq.next_element::<u16>()?.map(|u| u as u32),
                    4 => seq.next_element::<u32>()?,
                    _ => {
                        return Err(de::Error::invalid_value(
                            Unexpected::Unsigned(bytes_per_diff as u64),
                            &"bytes per diff",
                        ));
                    }
                } {
                    line_start = line_start + BytePos(diff);
                    lines.push(line_start);
                }
            }

            Ok(RefCell::new(lines))
        }
    }

    deserializer.deserialize_seq(LinesVisitor)
}

impl fmt::Debug for FileMap {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "FileMap({})", self.name)
    }
}

impl FileMap {
    /// EFFECT: register a start-of-line offset in the
    /// table of line-beginnings.
    /// UNCHECKED INVARIANT: these offsets must be added in the right
    /// order and must be in the right places; there is shared knowledge
    /// about what ends a line between this file and parse.rs
    /// WARNING: pos param here is the offset relative to start of CodeMap,
    /// and CodeMap will append a newline when adding a filemap without a newline at the end,
    /// so the safe way to call this is with value calculated as
    /// filemap.start_pos + newline_offset_relative_to_the_start_of_filemap.
    pub fn next_line(&self, pos: BytePos) {
        // the new charpos must be > the last one (or it's the first one).
        let mut lines = self.lines.borrow_mut();
        let line_len = lines.len();
        assert!(line_len == 0 || ((*lines)[line_len - 1] < pos));
        lines.push(pos);
    }

    /// get a line from the list of pre-computed line-beginnings.
    /// line-number here is 0-based.
    pub fn get_line(&self, line_number: usize) -> Option<&str> {
        match self.src {
            Some(ref src) => {
                let lines = self.lines.borrow();
                lines.get(line_number).map(|&line| {
                    let begin: BytePos = line - self.start_pos;
                    let begin = begin.to_usize();
                    // We can't use `lines.get(line_number+1)` because we might
                    // be parsing when we call this function and thus the current
                    // line is the last one we have line info for.
                    let slice = &src[begin..];
                    match slice.find('\n') {
                        Some(e) => &slice[..e],
                        None => slice,
                    }
                })
            }
            None => None,
        }
    }

    pub fn record_multibyte_char(&self, pos: BytePos, bytes: usize) {
        assert!(bytes >= 2 && bytes <= 4);
        let mbc = MultiByteChar { pos, bytes };
        self.multibyte_chars.borrow_mut().push(mbc);
    }

    pub fn is_real_file(&self) -> bool {
        !(self.name.starts_with('<') && self.name.ends_with('>'))
    }

    pub fn is_imported(&self) -> bool {
        self.src.is_none()
    }

    pub fn byte_length(&self) -> u32 {
        self.end_pos.0 - self.start_pos.0
    }
    pub fn count_lines(&self) -> usize {
        self.lines.borrow().len()
    }

    /// Find the line containing the given position. The return value is the
    /// index into the `lines` array of this FileMap, not the 1-based line
    /// number. If the filemap is empty or the position is located before the
    /// first line, None is returned.
    pub fn lookup_line(&self, pos: BytePos) -> Option<usize> {
        let lines = self.lines.borrow();
        if lines.len() == 0 {
            return None;
        }

        let line_index = lookup_line(&lines[..], pos);
        assert!(line_index < lines.len() as isize);
        if line_index >= 0 {
            Some(line_index as usize)
        } else {
            None
        }
    }

    pub fn line_bounds(&self, line_index: usize) -> (BytePos, BytePos) {
        if self.start_pos == self.end_pos {
            return (self.start_pos, self.end_pos);
        }

        let lines = self.lines.borrow();
        assert!(line_index < lines.len());
        if line_index == (lines.len() - 1) {
            (lines[line_index], self.end_pos)
        } else {
            (lines[line_index], lines[line_index + 1])
        }
    }
}

// _____________________________________________________________________________
// Pos, BytePos, CharPos
//

pub trait Pos {
    fn from_usize(n: usize) -> Self;
    fn to_usize(&self) -> usize;
}

/// A byte offset. Keep this small (currently 32-bits), as AST contains
/// a lot of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct BytePos(pub u32);

/// A character offset. Because of multibyte utf8 characters, a byte offset
/// is not equivalent to a character offset. The CodeMap will convert BytePos
/// values to CharPos values as necessary.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CharPos(pub usize);

// FIXME: Lots of boilerplate in these impls, but so far my attempts to fix
// have been unsuccessful

impl Pos for BytePos {
    fn from_usize(n: usize) -> BytePos {
        BytePos(n as u32)
    }
    fn to_usize(&self) -> usize {
        let BytePos(n) = *self;
        n as usize
    }
}

impl Add for BytePos {
    type Output = BytePos;

    fn add(self, rhs: BytePos) -> BytePos {
        BytePos((self.to_usize() + rhs.to_usize()) as u32)
    }
}

impl Sub for BytePos {
    type Output = BytePos;

    fn sub(self, rhs: BytePos) -> BytePos {
        BytePos((self.to_usize() - rhs.to_usize()) as u32)
    }
}

impl Pos for CharPos {
    fn from_usize(n: usize) -> CharPos {
        CharPos(n)
    }
    fn to_usize(&self) -> usize {
        let CharPos(n) = *self;
        n
    }
}

impl Add for CharPos {
    type Output = CharPos;

    fn add(self, rhs: CharPos) -> CharPos {
        CharPos(self.to_usize() + rhs.to_usize())
    }
}

impl Sub for CharPos {
    type Output = CharPos;

    fn sub(self, rhs: CharPos) -> CharPos {
        CharPos(self.to_usize() - rhs.to_usize())
    }
}

// _____________________________________________________________________________
// Loc, LocWithOpt, FileMapAndLine, FileMapAndBytePos
//

/// A source code location used for error reporting
#[derive(Debug, Clone)]
pub struct Loc {
    /// Information about the original source
    pub file: Rc<FileMap>,
    /// The (1-based) line number
    pub line: usize,
    /// The (0-based) column offset
    pub col: CharPos,
}

/// A source code location used as the result of lookup_char_pos_adj
// Actually, *none* of the clients use the filename *or* file field;
// perhaps they should just be removed.
#[derive(Debug)]
pub struct LocWithOpt {
    pub filename: FileName,
    pub line: usize,
    pub col: CharPos,
    pub file: Option<Rc<FileMap>>,
}

// used to be structural records. Better names, anyone?
#[derive(Debug)]
pub struct FileMapAndLine {
    pub fm: Rc<FileMap>,
    pub line: usize,
}
#[derive(Debug)]
pub struct FileMapAndBytePos {
    pub fm: Rc<FileMap>,
    pub pos: BytePos,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LineInfo {
    /// Index of line, starting from 0.
    pub line_index: usize,

    /// Column in line where span begins, starting from 0.
    pub start_col: CharPos,

    /// Column in line where span ends, starting from 0, exclusive.
    pub end_col: CharPos,
}

pub struct FileLines {
    pub file: Rc<FileMap>,
    pub lines: Vec<LineInfo>,
}

thread_local!(pub static SPAN_DEBUG: Cell<fn(Span, &mut fmt::Formatter) -> fmt::Result> =
                Cell::new(default_span_debug));

pub struct MacroBacktrace {
    /// span where macro was applied to generate this code
    pub call_site: Span,

    /// name of macro that was applied (e.g., "foo!" or "#[derive(Eq)]")
    pub macro_decl_name: String,

    /// span where macro was defined (if known)
    pub def_site_span: Option<Span>,
}

// _____________________________________________________________________________
// SpanLinesError, SpanSnippetError, DistinctSources, MalformedCodemapPositions
//

pub type FileLinesResult = Result<FileLines, SpanLinesError>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SpanLinesError {
    IllFormedSpan(Span),
    DistinctSources(DistinctSources),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SpanSnippetError {
    IllFormedSpan(Span),
    DistinctSources(DistinctSources),
    MalformedForCodemap(MalformedCodemapPositions),
    SourceNotAvailable { filename: String },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DistinctSources {
    pub begin: (String, BytePos),
    pub end: (String, BytePos),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MalformedCodemapPositions {
    pub name: String,
    pub source_len: usize,
    pub begin_pos: BytePos,
    pub end_pos: BytePos,
}

// Given a slice of line start positions and a position, returns the index of
// the line the position is on. Returns -1 if the position is located before
// the first line.
fn lookup_line(lines: &[BytePos], pos: BytePos) -> isize {
    match lines.binary_search(&pos) {
        Ok(line) => line as isize,
        Err(line) => line as isize - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{lookup_line, BytePos};

    #[test]
    fn test_lookup_line() {
        let lines = &[BytePos(3), BytePos(17), BytePos(28)];

        assert_eq!(lookup_line(lines, BytePos(0)), -1);
        assert_eq!(lookup_line(lines, BytePos(3)), 0);
        assert_eq!(lookup_line(lines, BytePos(4)), 0);

        assert_eq!(lookup_line(lines, BytePos(16)), 0);
        assert_eq!(lookup_line(lines, BytePos(17)), 1);
        assert_eq!(lookup_line(lines, BytePos(18)), 1);

        assert_eq!(lookup_line(lines, BytePos(28)), 2);
        assert_eq!(lookup_line(lines, BytePos(29)), 2);
    }
}
