//! Machinery for hygienic macros, inspired by the MTWT[1] paper.
//!
//! [1] Matthew Flatt, Ryan Culpepper, David Darais, and Robert Bruce Findler.
//! 2012. *Macros that work together: Compile-time bindings, partial expansion,
//! and definition contexts*. J. Funct. Program. 22, 2 (March 2012), 181-216.
//! DOI=10.1017/S0956796812000093 http://dx.doi.org/10.1017/S0956796812000093

use crate::symbol::{Ident, Symbol};
use crate::Span;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A SyntaxContext represents a chain of macro expansions (represented by marks).
#[derive(Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub struct SyntaxContext(u32);

pub const NO_EXPANSION: SyntaxContext = SyntaxContext(0);

#[derive(Copy, Clone, Default)]
pub struct SyntaxContextData {
    pub outer_mark: Mark,
    pub prev_ctxt: SyntaxContext,
    pub modern: SyntaxContext,
}

/// A mark is a unique id associated with a macro expansion.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Mark(u32);

#[derive(Default)]
struct MarkData {
    parent: Mark,
    modern: bool,
    expn_info: Option<ExpnInfo>,
}

impl Mark {
    pub fn fresh(parent: Mark) -> Self {
        HygieneData::with(|data| {
            data.marks.push(MarkData {
                parent,
                modern: false,
                expn_info: None,
            });
            Mark(data.marks.len() as u32 - 1)
        })
    }

    /// The mark of the theoretical expansion that generates freshly parsed, unexpanded AST.
    pub fn root() -> Self {
        Mark(0)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }

    pub fn from_u32(raw: u32) -> Mark {
        Mark(raw)
    }

    pub fn expn_info(self) -> Option<ExpnInfo> {
        HygieneData::with(|data| data.marks[self.0 as usize].expn_info.clone())
    }

    pub fn set_expn_info(self, info: ExpnInfo) {
        HygieneData::with(|data| data.marks[self.0 as usize].expn_info = Some(info))
    }

    pub fn modern(mut self) -> Mark {
        HygieneData::with(|data| loop {
            if self == Mark::root() || data.marks[self.0 as usize].modern {
                return self;
            }
            self = data.marks[self.0 as usize].parent;
        })
    }

    pub fn is_modern(self) -> bool {
        HygieneData::with(|data| data.marks[self.0 as usize].modern)
    }

    pub fn set_modern(self) {
        HygieneData::with(|data| data.marks[self.0 as usize].modern = true)
    }

    pub fn is_descendant_of(mut self, ancestor: Mark) -> bool {
        HygieneData::with(|data| {
            while self != ancestor {
                if self == Mark::root() {
                    return false;
                }
                self = data.marks[self.0 as usize].parent;
            }
            true
        })
    }
}

struct HygieneData {
    marks: Vec<MarkData>,
    syntax_contexts: Vec<SyntaxContextData>,
    markings: HashMap<(SyntaxContext, Mark), SyntaxContext>,
    gensym_to_ctxt: HashMap<Symbol, SyntaxContext>,
}

impl HygieneData {
    fn new() -> Self {
        HygieneData {
            marks: vec![MarkData::default()],
            syntax_contexts: vec![SyntaxContextData::default()],
            markings: HashMap::new(),
            gensym_to_ctxt: HashMap::new(),
        }
    }

    fn with<T, F: FnOnce(&mut HygieneData) -> T>(f: F) -> T {
        thread_local! {
            static HYGIENE_DATA: RefCell<HygieneData> = RefCell::new(HygieneData::new());
        }
        HYGIENE_DATA.with(|data| f(&mut *data.borrow_mut()))
    }
}

pub fn clear_markings() {
    HygieneData::with(|data| data.markings = HashMap::new());
}

impl SyntaxContext {
    pub fn empty() -> Self {
        NO_EXPANSION
    }

    /// Extend a syntax context with a given mark
    pub fn apply_mark(self, mark: Mark) -> SyntaxContext {
        HygieneData::with(|data| {
            let syntax_contexts = &mut data.syntax_contexts;
            let ctxt_data = syntax_contexts[self.0 as usize];
            if mark == ctxt_data.outer_mark {
                return ctxt_data.prev_ctxt;
            }

            let modern = if data.marks[mark.0 as usize].modern {
                *data
                    .markings
                    .entry((ctxt_data.modern, mark))
                    .or_insert_with(|| {
                        let modern = SyntaxContext(syntax_contexts.len() as u32);
                        syntax_contexts.push(SyntaxContextData {
                            outer_mark: mark,
                            prev_ctxt: ctxt_data.modern,
                            modern,
                        });
                        modern
                    })
            } else {
                ctxt_data.modern
            };

            *data.markings.entry((self, mark)).or_insert_with(|| {
                syntax_contexts.push(SyntaxContextData {
                    outer_mark: mark,
                    prev_ctxt: self,
                    modern,
                });
                SyntaxContext(syntax_contexts.len() as u32 - 1)
            })
        })
    }

    pub fn remove_mark(&mut self) -> Mark {
        HygieneData::with(|data| {
            let outer_mark = data.syntax_contexts[self.0 as usize].outer_mark;
            *self = data.syntax_contexts[self.0 as usize].prev_ctxt;
            outer_mark
        })
    }

    /// Adjust this context for resolution in a scope created by the given expansion.
    /// This returns the expansion whose definition scope we use to privacy check the resolution,
    /// or `None` if we privacy check as usual (i.e. not w.r.t. a macro definition scope).
    pub fn adjust(&mut self, expansion: Mark) -> Option<Mark> {
        let mut scope = None;
        while !expansion.is_descendant_of(self.outer()) {
            scope = Some(self.remove_mark());
        }
        scope
    }

    /// Adjust this context for resolution in a scope created by the given expansion
    /// via a glob import with the given `SyntaxContext`.
    /// This returns `None` if the context cannot be glob-adjusted.
    /// Otherwise, it returns the scope to use when privacy checking (see `adjust` for details).
    pub fn glob_adjust(
        &mut self,
        expansion: Mark,
        mut glob_ctxt: SyntaxContext,
    ) -> Option<Option<Mark>> {
        let mut scope = None;
        while !expansion.is_descendant_of(glob_ctxt.outer()) {
            scope = Some(glob_ctxt.remove_mark());
            if self.remove_mark() != scope.unwrap() {
                return None;
            }
        }
        if self.adjust(expansion).is_some() {
            return None;
        }
        Some(scope)
    }

    /// Undo `glob_adjust` if possible:
    pub fn reverse_glob_adjust(
        &mut self,
        expansion: Mark,
        mut glob_ctxt: SyntaxContext,
    ) -> Option<Option<Mark>> {
        if self.adjust(expansion).is_some() {
            return None;
        }

        let mut marks = Vec::new();
        while !expansion.is_descendant_of(glob_ctxt.outer()) {
            marks.push(glob_ctxt.remove_mark());
        }

        let scope = marks.last().cloned();
        while let Some(mark) = marks.pop() {
            *self = self.apply_mark(mark);
        }
        Some(scope)
    }

    pub fn modern(self) -> SyntaxContext {
        HygieneData::with(|data| data.syntax_contexts[self.0 as usize].modern)
    }

    pub fn outer(self) -> Mark {
        HygieneData::with(|data| data.syntax_contexts[self.0 as usize].outer_mark)
    }
}

impl fmt::Debug for SyntaxContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Extra information for tracking spans of macro and syntax sugar expansion
#[derive(Clone, Hash, Debug)]
pub struct ExpnInfo {
    /// The location of the actual macro invocation or syntax sugar , e.g.
    /// `let x = foo!();` or `if let Some(y) = x {}`
    ///
    /// This may recursively refer to other macro invocations, e.g. if
    /// `foo!()` invoked `bar!()` internally, and there was an
    /// expression inside `bar!`; the call_site of the expression in
    /// the expansion would point to the `bar!` invocation; that
    /// call_site span would have its own ExpnInfo, with the call_site
    /// pointing to the `foo!` invocation.
    pub call_site: Span,
    /// Information about the expansion.
    pub callee: NameAndSpan,
}

#[derive(Clone, Hash, Debug)]
pub struct NameAndSpan {
    /// The format with which the macro was invoked.
    pub format: ExpnFormat,
    /// Whether the macro is allowed to use #[unstable]/feature-gated
    /// features internally without forcing the whole crate to opt-in
    /// to them.
    pub allow_internal_unstable: bool,
    /// The span of the macro definition itself. The macro may not
    /// have a sensible definition span (e.g. something defined
    /// completely inside libsyntax) in which case this is None.
    pub span: Option<Span>,
}

impl NameAndSpan {
    pub fn name(&self) -> Symbol {
        match self.format {
            ExpnFormat::MacroAttribute(s)
            | ExpnFormat::MacroBang(s)
            | ExpnFormat::CompilerDesugaring(s) => s,
        }
    }
}

/// The source of expansion.
#[derive(Clone, Hash, Debug, PartialEq, Eq)]
pub enum ExpnFormat {
    /// e.g. #[derive(...)] <item>
    MacroAttribute(Symbol),
    /// e.g. `format!()`
    MacroBang(Symbol),
    /// Desugaring done by the compiler during HIR lowering.
    CompilerDesugaring(Symbol),
}

impl Serialize for SyntaxContext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // FIXME(jseyfried) intercrate hygiene
        serializer.serialize_unit()
    }
}

impl<'de> Deserialize<'de> for SyntaxContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // FIXME(jseyfried) intercrate hygiene
        Deserialize::deserialize(deserializer).map(|()| SyntaxContext::empty())
    }
}

impl Symbol {
    pub fn from_ident(ident: Ident) -> Symbol {
        HygieneData::with(|data| {
            let gensym = ident.name.gensymed();
            data.gensym_to_ctxt.insert(gensym, ident.ctxt);
            gensym
        })
    }

    pub fn to_ident(self) -> Ident {
        HygieneData::with(|data| match data.gensym_to_ctxt.get(&self) {
            Some(&ctxt) => Ident {
                name: self.interned(),
                ctxt,
            },
            None => Ident::with_empty_ctxt(self),
        })
    }
}
