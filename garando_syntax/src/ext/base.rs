pub use self::SyntaxExtension::{IdentTT, MultiDecorator, MultiModifier, NormalTT};

use crate::ast::{self, Attribute, MetaItem, Name, PatKind};
use crate::attr::HasAttrs;
use crate::codemap::{self, respan, CodeMap, Spanned};
use crate::errors::DiagnosticBuilder;
use crate::ext::expand::{self, Expansion, Invocation};
use crate::ext::hygiene::{Mark, SyntaxContext};
use crate::fold::{self, Folder};
use crate::parse::token;
use crate::parse::{self, parser, DirectoryOwnership};
use crate::ptr::P;
use crate::symbol::Symbol;
use crate::syntax_pos::{Span, DUMMY_SP};
use crate::util::small_vector::SmallVector;

use crate::tokenstream::{self, TokenStream};
use std::collections::HashMap;
use std::default::Default;
use std::path::PathBuf;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub enum Annotatable {
    Item(P<ast::Item>),
    TraitItem(P<ast::TraitItem>),
    ImplItem(P<ast::ImplItem>),
}

impl HasAttrs for Annotatable {
    fn attrs(&self) -> &[Attribute] {
        match *self {
            Annotatable::Item(ref item) => &item.attrs,
            Annotatable::TraitItem(ref trait_item) => &trait_item.attrs,
            Annotatable::ImplItem(ref impl_item) => &impl_item.attrs,
        }
    }

    fn map_attrs<F: FnOnce(Vec<Attribute>) -> Vec<Attribute>>(self, f: F) -> Self {
        match self {
            Annotatable::Item(item) => Annotatable::Item(item.map_attrs(f)),
            Annotatable::TraitItem(trait_item) => Annotatable::TraitItem(trait_item.map_attrs(f)),
            Annotatable::ImplItem(impl_item) => Annotatable::ImplItem(impl_item.map_attrs(f)),
        }
    }
}

impl Annotatable {
    pub fn span(&self) -> Span {
        match *self {
            Annotatable::Item(ref item) => item.span,
            Annotatable::TraitItem(ref trait_item) => trait_item.span,
            Annotatable::ImplItem(ref impl_item) => impl_item.span,
        }
    }

    pub fn expect_item(self) -> P<ast::Item> {
        match self {
            Annotatable::Item(i) => i,
            _ => panic!("expected Item"),
        }
    }

    pub fn map_item_or<F, G>(self, mut f: F, mut or: G) -> Annotatable
    where
        F: FnMut(P<ast::Item>) -> P<ast::Item>,
        G: FnMut(Annotatable) -> Annotatable,
    {
        match self {
            Annotatable::Item(i) => Annotatable::Item(f(i)),
            _ => or(self),
        }
    }

    pub fn expect_trait_item(self) -> ast::TraitItem {
        match self {
            Annotatable::TraitItem(i) => i.unwrap(),
            _ => panic!("expected Item"),
        }
    }

    pub fn expect_impl_item(self) -> ast::ImplItem {
        match self {
            Annotatable::ImplItem(i) => i.unwrap(),
            _ => panic!("expected Item"),
        }
    }
}

// A more flexible ItemDecorator.
pub trait MultiItemDecorator {
    fn expand(
        &self,
        ecx: &mut ExtCtxt,
        sp: Span,
        meta_item: &ast::MetaItem,
        item: &Annotatable,
        push: &mut dyn FnMut(Annotatable),
    );
}

impl<F> MultiItemDecorator for F
where
    F: Fn(&mut ExtCtxt, Span, &ast::MetaItem, &Annotatable, &mut dyn FnMut(Annotatable)),
{
    fn expand(
        &self,
        ecx: &mut ExtCtxt,
        sp: Span,
        meta_item: &ast::MetaItem,
        item: &Annotatable,
        push: &mut dyn FnMut(Annotatable),
    ) {
        (*self)(ecx, sp, meta_item, item, push)
    }
}

// `meta_item` is the annotation, and `item` is the item being modified.
// FIXME Decorators should follow the same pattern too.
pub trait MultiItemModifier {
    fn expand(
        &self,
        ecx: &mut ExtCtxt,
        span: Span,
        meta_item: &ast::MetaItem,
        item: Annotatable,
    ) -> Vec<Annotatable>;
}

impl<F, T> MultiItemModifier for F
where
    F: Fn(&mut ExtCtxt, Span, &ast::MetaItem, Annotatable) -> T,
    T: Into<Vec<Annotatable>>,
{
    fn expand(
        &self,
        ecx: &mut ExtCtxt,
        span: Span,
        meta_item: &ast::MetaItem,
        item: Annotatable,
    ) -> Vec<Annotatable> {
        (*self)(ecx, span, meta_item, item).into()
    }
}

impl Into<Vec<Annotatable>> for Annotatable {
    fn into(self) -> Vec<Annotatable> {
        vec![self]
    }
}

pub trait ProcMacro {
    fn expand<'cx>(&self, ecx: &'cx mut ExtCtxt, span: Span, ts: TokenStream) -> TokenStream;
}

impl<F> ProcMacro for F
where
    F: Fn(TokenStream) -> TokenStream,
{
    fn expand<'cx>(&self, _ecx: &'cx mut ExtCtxt, _span: Span, ts: TokenStream) -> TokenStream {
        // FIXME setup implicit context in TLS before calling self.
        (*self)(ts)
    }
}

pub trait AttrProcMacro {
    fn expand<'cx>(
        &self,
        ecx: &'cx mut ExtCtxt,
        span: Span,
        annotation: TokenStream,
        annotated: TokenStream,
    ) -> TokenStream;
}

impl<F> AttrProcMacro for F
where
    F: Fn(TokenStream, TokenStream) -> TokenStream,
{
    fn expand<'cx>(
        &self,
        _ecx: &'cx mut ExtCtxt,
        _span: Span,
        annotation: TokenStream,
        annotated: TokenStream,
    ) -> TokenStream {
        // FIXME setup implicit context in TLS before calling self.
        (*self)(annotation, annotated)
    }
}

/// Represents a thing that maps token trees to Macro Results
pub trait TTMacroExpander {
    fn expand<'cx>(
        &self,
        ecx: &'cx mut ExtCtxt,
        span: Span,
        input: TokenStream,
    ) -> Box<dyn MacResult + 'cx>;
}

pub type MacroExpanderFn =
    for<'cx> fn(&'cx mut ExtCtxt, Span, &[tokenstream::TokenTree]) -> Box<dyn MacResult + 'cx>;

impl<F> TTMacroExpander for F
where
    F: for<'cx> Fn(&'cx mut ExtCtxt, Span, &[tokenstream::TokenTree]) -> Box<dyn MacResult + 'cx>,
{
    fn expand<'cx>(
        &self,
        ecx: &'cx mut ExtCtxt,
        span: Span,
        input: TokenStream,
    ) -> Box<dyn MacResult + 'cx> {
        struct AvoidInterpolatedIdents;

        impl Folder for AvoidInterpolatedIdents {
            fn fold_tt(&mut self, tt: tokenstream::TokenTree) -> tokenstream::TokenTree {
                if let tokenstream::TokenTree::Token(_, token::Interpolated(ref nt)) = tt {
                    if let token::NtIdent(ident) = **nt {
                        return tokenstream::TokenTree::Token(ident.span, token::Ident(ident.node));
                    }
                }
                fold::noop_fold_tt(tt, self)
            }

            fn fold_mac(&mut self, mac: ast::Mac) -> ast::Mac {
                fold::noop_fold_mac(mac, self)
            }
        }

        let input: Vec<_> = input
            .trees()
            .map(|tt| AvoidInterpolatedIdents.fold_tt(tt))
            .collect();
        (*self)(ecx, span, &input)
    }
}

pub trait IdentMacroExpander {
    fn expand<'cx>(
        &self,
        cx: &'cx mut ExtCtxt,
        sp: Span,
        ident: ast::Ident,
        token_tree: Vec<tokenstream::TokenTree>,
    ) -> Box<dyn MacResult + 'cx>;
}

pub type IdentMacroExpanderFn = for<'cx> fn(
    &'cx mut ExtCtxt,
    Span,
    ast::Ident,
    Vec<tokenstream::TokenTree>,
) -> Box<dyn MacResult + 'cx>;

impl<F> IdentMacroExpander for F
where
    F: for<'cx> Fn(
        &'cx mut ExtCtxt,
        Span,
        ast::Ident,
        Vec<tokenstream::TokenTree>,
    ) -> Box<dyn MacResult + 'cx>,
{
    fn expand<'cx>(
        &self,
        cx: &'cx mut ExtCtxt,
        sp: Span,
        ident: ast::Ident,
        token_tree: Vec<tokenstream::TokenTree>,
    ) -> Box<dyn MacResult + 'cx> {
        (*self)(cx, sp, ident, token_tree)
    }
}

// Use a macro because forwarding to a simple function has type system issues
macro_rules! make_stmts_default {
    ($me:expr) => {
        $me.make_expr().map(|e| {
            SmallVector::one(ast::Stmt {
                id: ast::DUMMY_NODE_ID,
                span: e.span,
                node: ast::StmtKind::Expr(e),
            })
        })
    };
}

/// The result of a macro expansion. The return values of the various
/// methods are spliced into the AST at the callsite of the macro.
pub trait MacResult {
    /// Create an expression.
    fn make_expr(self: Box<Self>) -> Option<P<ast::Expr>> {
        None
    }
    /// Create zero or more items.
    fn make_items(self: Box<Self>) -> Option<SmallVector<P<ast::Item>>> {
        None
    }

    /// Create zero or more impl items.
    fn make_impl_items(self: Box<Self>) -> Option<SmallVector<ast::ImplItem>> {
        None
    }

    /// Create zero or more trait items.
    fn make_trait_items(self: Box<Self>) -> Option<SmallVector<ast::TraitItem>> {
        None
    }

    /// Create a pattern.
    fn make_pat(self: Box<Self>) -> Option<P<ast::Pat>> {
        None
    }

    /// Create zero or more statements.
    ///
    /// By default this attempts to create an expression statement,
    /// returning None if that fails.
    fn make_stmts(self: Box<Self>) -> Option<SmallVector<ast::Stmt>> {
        make_stmts_default!(self)
    }

    fn make_ty(self: Box<Self>) -> Option<P<ast::Ty>> {
        None
    }
}

macro_rules! make_MacEager {
    ( $( $fld:ident: $t:ty, )* ) => {
        /// `MacResult` implementation for the common case where you've already
        /// built each form of AST that you might return.
        #[derive(Default)]
        pub struct MacEager {
            $(
                pub $fld: Option<$t>,
            )*
        }

        impl MacEager {
            $(
                pub fn $fld(v: $t) -> Box<dyn MacResult> {
                    Box::new(MacEager {
                        $fld: Some(v),
                        ..Default::default()
                    })
                }
            )*
        }
    }
}

make_MacEager! {
    expr: P<ast::Expr>,
    pat: P<ast::Pat>,
    items: SmallVector<P<ast::Item>>,
    impl_items: SmallVector<ast::ImplItem>,
    trait_items: SmallVector<ast::TraitItem>,
    stmts: SmallVector<ast::Stmt>,
    ty: P<ast::Ty>,
}

impl MacResult for MacEager {
    fn make_expr(self: Box<Self>) -> Option<P<ast::Expr>> {
        self.expr
    }

    fn make_items(self: Box<Self>) -> Option<SmallVector<P<ast::Item>>> {
        self.items
    }

    fn make_impl_items(self: Box<Self>) -> Option<SmallVector<ast::ImplItem>> {
        self.impl_items
    }

    fn make_trait_items(self: Box<Self>) -> Option<SmallVector<ast::TraitItem>> {
        self.trait_items
    }

    fn make_stmts(self: Box<Self>) -> Option<SmallVector<ast::Stmt>> {
        match self.stmts.as_ref().map_or(0, |s| s.len()) {
            0 => make_stmts_default!(self),
            _ => self.stmts,
        }
    }

    fn make_pat(self: Box<Self>) -> Option<P<ast::Pat>> {
        if let Some(p) = self.pat {
            return Some(p);
        }
        if let Some(e) = self.expr {
            if let ast::ExprKind::Lit(_) = e.node {
                return Some(P(ast::Pat {
                    id: ast::DUMMY_NODE_ID,
                    span: e.span,
                    node: PatKind::Lit(e),
                }));
            }
        }
        None
    }

    fn make_ty(self: Box<Self>) -> Option<P<ast::Ty>> {
        self.ty
    }
}

/// Fill-in macro expansion result, to allow compilation to continue
/// after hitting errors.
#[derive(Copy, Clone)]
pub struct DummyResult {
    expr_only: bool,
    span: Span,
}

impl DummyResult {
    /// Create a default MacResult that can be anything.
    ///
    /// Use this as a return value after hitting any errors and
    /// calling `span_err`.
    pub fn any(sp: Span) -> Box<dyn MacResult + 'static> {
        Box::new(DummyResult {
            expr_only: false,
            span: sp,
        })
    }

    /// Create a default MacResult that can only be an expression.
    ///
    /// Use this for macros that must expand to an expression, so even
    /// if an error is encountered internally, the user will receive
    /// an error that they also used it in the wrong place.
    pub fn expr(sp: Span) -> Box<dyn MacResult + 'static> {
        Box::new(DummyResult {
            expr_only: true,
            span: sp,
        })
    }

    /// A plain dummy expression.
    pub fn raw_expr(sp: Span) -> P<ast::Expr> {
        P(ast::Expr {
            id: ast::DUMMY_NODE_ID,
            node: ast::ExprKind::Lit(P(codemap::respan(sp, ast::LitKind::Bool(false)))),
            span: sp,
            attrs: ast::ThinVec::new(),
        })
    }

    /// A plain dummy pattern.
    pub fn raw_pat(sp: Span) -> ast::Pat {
        ast::Pat {
            id: ast::DUMMY_NODE_ID,
            node: PatKind::Wild,
            span: sp,
        }
    }

    pub fn raw_ty(sp: Span) -> P<ast::Ty> {
        P(ast::Ty {
            id: ast::DUMMY_NODE_ID,
            node: ast::TyKind::Infer,
            span: sp,
        })
    }
}

impl MacResult for DummyResult {
    fn make_expr(self: Box<DummyResult>) -> Option<P<ast::Expr>> {
        Some(DummyResult::raw_expr(self.span))
    }

    fn make_pat(self: Box<DummyResult>) -> Option<P<ast::Pat>> {
        Some(P(DummyResult::raw_pat(self.span)))
    }

    fn make_items(self: Box<DummyResult>) -> Option<SmallVector<P<ast::Item>>> {
        // this code needs a comment... why not always just return the Some() ?
        if self.expr_only {
            None
        } else {
            Some(SmallVector::new())
        }
    }

    fn make_impl_items(self: Box<DummyResult>) -> Option<SmallVector<ast::ImplItem>> {
        if self.expr_only {
            None
        } else {
            Some(SmallVector::new())
        }
    }

    fn make_trait_items(self: Box<DummyResult>) -> Option<SmallVector<ast::TraitItem>> {
        if self.expr_only {
            None
        } else {
            Some(SmallVector::new())
        }
    }

    fn make_stmts(self: Box<DummyResult>) -> Option<SmallVector<ast::Stmt>> {
        Some(SmallVector::one(ast::Stmt {
            id: ast::DUMMY_NODE_ID,
            node: ast::StmtKind::Expr(DummyResult::raw_expr(self.span)),
            span: self.span,
        }))
    }

    fn make_ty(self: Box<DummyResult>) -> Option<P<ast::Ty>> {
        Some(DummyResult::raw_ty(self.span))
    }
}

pub type BuiltinDeriveFn =
    for<'cx> fn(&'cx mut ExtCtxt, Span, &MetaItem, &Annotatable, &mut dyn FnMut(Annotatable));

/// Represents different kinds of macro invocations that can be resolved.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Debug)]
pub enum MacroKind {
    /// A bang macro - foo!()
    Bang,
    /// An attribute macro - #[foo]
    Attr,
    /// A derive attribute macro - #[derive(Foo)]
    Derive,
}

/// An enum representing the different kinds of syntax extensions.
pub enum SyntaxExtension {
    /// A syntax extension that is attached to an item and creates new items
    /// based upon it.
    ///
    /// `#[derive(...)]` is a `MultiItemDecorator`.
    ///
    /// Prefer ProcMacro or MultiModifier since they are more flexible.
    MultiDecorator(Box<dyn MultiItemDecorator>),

    /// A syntax extension that is attached to an item and modifies it
    /// in-place. Also allows decoration, i.e., creating new items.
    MultiModifier(Box<dyn MultiItemModifier>),

    /// A function-like procedural macro. TokenStream -> TokenStream.
    ProcMacro(Box<dyn ProcMacro>),

    /// An attribute-like procedural macro. TokenStream, TokenStream -> TokenStream.
    /// The first TokenSteam is the attribute, the second is the annotated item.
    /// Allows modification of the input items and adding new items, similar to
    /// MultiModifier, but uses TokenStreams, rather than AST nodes.
    AttrProcMacro(Box<dyn AttrProcMacro>),

    /// A normal, function-like syntax extension.
    ///
    /// `bytes!` is a `NormalTT`.
    ///
    /// The `bool` dictates whether the contents of the macro can
    /// directly use `#[unstable]` things (true == yes).
    NormalTT(Box<dyn TTMacroExpander>, Option<(ast::NodeId, Span)>, bool),

    /// A function-like syntax extension that has an extra ident before
    /// the block.
    ///
    IdentTT(Box<dyn IdentMacroExpander>, Option<Span>, bool),

    /// An attribute-like procedural macro. TokenStream -> TokenStream.
    /// The input is the annotated item.
    /// Allows generating code to implement a Trait for a given struct
    /// or enum item.
    ProcMacroDerive(
        Box<dyn MultiItemModifier>,
        Vec<Symbol>, /* inert attribute names */
    ),

    /// An attribute-like procedural macro that derives a builtin trait.
    BuiltinDerive(BuiltinDeriveFn),

    /// A declarative macro, e.g. `macro m() {}`.
    DeclMacro(
        Box<dyn TTMacroExpander>,
        Option<Span>, /* definition site span */
    ),
}

impl SyntaxExtension {
    /// Return which kind of macro calls this syntax extension.
    pub fn kind(&self) -> MacroKind {
        match *self {
            SyntaxExtension::DeclMacro(..)
            | SyntaxExtension::NormalTT(..)
            | SyntaxExtension::IdentTT(..)
            | SyntaxExtension::ProcMacro(..) => MacroKind::Bang,
            SyntaxExtension::MultiDecorator(..)
            | SyntaxExtension::MultiModifier(..)
            | SyntaxExtension::AttrProcMacro(..) => MacroKind::Attr,
            SyntaxExtension::ProcMacroDerive(..) | SyntaxExtension::BuiltinDerive(..) => {
                MacroKind::Derive
            }
        }
    }

    pub fn is_modern(&self) -> bool {
        match *self {
            SyntaxExtension::DeclMacro(..) => true,
            _ => false,
        }
    }
}

pub type NamedSyntaxExtension = (Name, SyntaxExtension);

pub trait Resolver {
    fn next_node_id(&mut self) -> ast::NodeId;
    fn get_module_scope(&mut self, id: ast::NodeId) -> Mark;
    fn eliminate_crate_var(&mut self, item: P<ast::Item>) -> P<ast::Item>;
    fn is_whitelisted_legacy_custom_derive(&self, name: Name) -> bool;

    fn visit_expansion(&mut self, mark: Mark, expansion: &Expansion, derives: &[Mark]);
    fn add_builtin(&mut self, ident: ast::Ident, ext: Rc<SyntaxExtension>);

    fn resolve_imports(&mut self);
    // Resolves attribute and derive legacy macros from `#![plugin(..)]`.
    fn find_legacy_attr_invoc(&mut self, attrs: &mut Vec<Attribute>) -> Option<Attribute>;
    fn resolve_invoc(
        &mut self,
        invoc: &mut Invocation,
        scope: Mark,
        force: bool,
    ) -> Result<Option<Rc<SyntaxExtension>>, Determinacy>;
    fn resolve_macro(
        &mut self,
        scope: Mark,
        path: &ast::Path,
        kind: MacroKind,
        force: bool,
    ) -> Result<Rc<SyntaxExtension>, Determinacy>;
    fn check_unused_macros(&self);
}

#[derive(Copy, Clone, Debug)]
pub enum Determinacy {
    Determined,
    Undetermined,
}

pub struct DummyResolver;

impl Resolver for DummyResolver {
    fn next_node_id(&mut self) -> ast::NodeId {
        ast::DUMMY_NODE_ID
    }
    fn get_module_scope(&mut self, _id: ast::NodeId) -> Mark {
        Mark::root()
    }
    fn eliminate_crate_var(&mut self, item: P<ast::Item>) -> P<ast::Item> {
        item
    }
    fn is_whitelisted_legacy_custom_derive(&self, _name: Name) -> bool {
        false
    }

    fn visit_expansion(&mut self, _invoc: Mark, _expansion: &Expansion, _derives: &[Mark]) {}
    fn add_builtin(&mut self, _ident: ast::Ident, _ext: Rc<SyntaxExtension>) {}

    fn resolve_imports(&mut self) {}
    fn find_legacy_attr_invoc(&mut self, _attrs: &mut Vec<Attribute>) -> Option<Attribute> {
        None
    }
    fn resolve_invoc(
        &mut self,
        _invoc: &mut Invocation,
        _scope: Mark,
        _force: bool,
    ) -> Result<Option<Rc<SyntaxExtension>>, Determinacy> {
        Err(Determinacy::Determined)
    }
    fn resolve_macro(
        &mut self,
        _scope: Mark,
        _path: &ast::Path,
        _kind: MacroKind,
        _force: bool,
    ) -> Result<Rc<SyntaxExtension>, Determinacy> {
        Err(Determinacy::Determined)
    }
    fn check_unused_macros(&self) {}
}

#[derive(Clone)]
pub struct ModuleData {
    pub mod_path: Vec<ast::Ident>,
    pub directory: PathBuf,
}

#[derive(Clone)]
pub struct ExpansionData {
    pub mark: Mark,
    pub depth: usize,
    pub module: Rc<ModuleData>,
    pub directory_ownership: DirectoryOwnership,
}

/// One of these is made during expansion and incrementally updated as we go;
/// when a macro expansion occurs, the resulting nodes have the `backtrace()
/// -> expn_info` of their expansion context stored into their span.
pub struct ExtCtxt<'a> {
    pub parse_sess: &'a parse::ParseSess,
    pub ecfg: expand::ExpansionConfig<'a>,
    pub crate_root: Option<&'static str>,
    pub resolver: &'a mut dyn Resolver,
    pub resolve_err_count: usize,
    pub current_expansion: ExpansionData,
    pub expansions: HashMap<Span, Vec<String>>,
}

impl<'a> ExtCtxt<'a> {
    pub fn new(
        parse_sess: &'a parse::ParseSess,
        ecfg: expand::ExpansionConfig<'a>,
        resolver: &'a mut dyn Resolver,
    ) -> ExtCtxt<'a> {
        ExtCtxt {
            parse_sess: parse_sess,
            ecfg: ecfg,
            crate_root: None,
            resolver: resolver,
            resolve_err_count: 0,
            current_expansion: ExpansionData {
                mark: Mark::root(),
                depth: 0,
                module: Rc::new(ModuleData {
                    mod_path: Vec::new(),
                    directory: PathBuf::new(),
                }),
                directory_ownership: DirectoryOwnership::Owned,
            },
            expansions: HashMap::new(),
        }
    }

    /// Returns a `Folder` for deeply expanding all macros in an AST node.
    pub fn expander<'b>(&'b mut self) -> expand::MacroExpander<'b, 'a> {
        expand::MacroExpander::new(self, false)
    }

    /// Returns a `Folder` that deeply expands all macros and assigns all node ids in an AST node.
    /// Once node ids are assigned, the node may not be expanded, removed, or otherwise modified.
    pub fn monotonic_expander<'b>(&'b mut self) -> expand::MacroExpander<'b, 'a> {
        expand::MacroExpander::new(self, true)
    }

    pub fn new_parser_from_tts(&self, tts: &[tokenstream::TokenTree]) -> parser::Parser<'a> {
        parse::stream_to_parser(self.parse_sess, tts.iter().cloned().collect())
    }
    pub fn codemap(&self) -> &'a CodeMap {
        self.parse_sess.codemap()
    }
    pub fn parse_sess(&self) -> &'a parse::ParseSess {
        self.parse_sess
    }
    pub fn cfg(&self) -> &ast::CrateConfig {
        &self.parse_sess.config
    }
    pub fn call_site(&self) -> Span {
        match self.current_expansion.mark.expn_info() {
            Some(expn_info) => expn_info.call_site,
            None => DUMMY_SP,
        }
    }
    pub fn backtrace(&self) -> SyntaxContext {
        SyntaxContext::empty().apply_mark(self.current_expansion.mark)
    }

    /// Returns span for the macro which originally caused the current expansion to happen.
    ///
    /// Stops backtracing at include! boundary.
    pub fn expansion_cause(&self) -> Option<Span> {
        let mut ctxt = self.backtrace();
        let mut last_macro = None;
        loop {
            if ctxt
                .outer()
                .expn_info()
                .map_or(None, |info| {
                    if info.callee.name() == "include" {
                        // Stop going up the backtrace once include! is encountered
                        return None;
                    }
                    ctxt = info.call_site.ctxt;
                    last_macro = Some(info.call_site);
                    Some(())
                })
                .is_none()
            {
                break;
            }
        }
        last_macro
    }

    pub fn struct_span_warn(&self, sp: Span, msg: &str) -> DiagnosticBuilder<'a> {
        self.parse_sess.span_diagnostic.struct_span_warn(sp, msg)
    }
    pub fn struct_span_err(&self, sp: Span, msg: &str) -> DiagnosticBuilder<'a> {
        self.parse_sess.span_diagnostic.struct_span_err(sp, msg)
    }
    pub fn struct_span_fatal(&self, sp: Span, msg: &str) -> DiagnosticBuilder<'a> {
        self.parse_sess.span_diagnostic.struct_span_fatal(sp, msg)
    }

    /// Emit `msg` attached to `sp`, and stop compilation immediately.
    ///
    /// `span_err` should be strongly preferred where-ever possible:
    /// this should *only* be used when
    /// - continuing has a high risk of flow-on errors (e.g. errors in
    ///   declaring a macro would cause all uses of that macro to
    ///   complain about "undefined macro"), or
    /// - there is literally nothing else that can be done (however,
    ///   in most cases one can construct a dummy expression/item to
    ///   substitute; we never hit resolve/type-checking so the dummy
    ///   value doesn't have to match anything)
    pub fn span_fatal(&self, sp: Span, msg: &str) -> ! {
        panic!(self.parse_sess.span_diagnostic.span_fatal(sp, msg));
    }

    /// Emit `msg` attached to `sp`, without immediately stopping
    /// compilation.
    ///
    /// Compilation will be stopped in the near future (at the end of
    /// the macro expansion phase).
    pub fn span_err(&self, sp: Span, msg: &str) {
        self.parse_sess.span_diagnostic.span_err(sp, msg);
    }
    pub fn span_warn(&self, sp: Span, msg: &str) {
        self.parse_sess.span_diagnostic.span_warn(sp, msg);
    }
    pub fn span_unimpl(&self, sp: Span, msg: &str) -> ! {
        self.parse_sess.span_diagnostic.span_unimpl(sp, msg);
    }
    pub fn span_bug(&self, sp: Span, msg: &str) -> ! {
        self.parse_sess.span_diagnostic.span_bug(sp, msg);
    }
    pub fn trace_macros_diag(&self) {
        for (sp, notes) in self.expansions.iter() {
            let mut db = self
                .parse_sess
                .span_diagnostic
                .span_note_diag(*sp, "trace_macro");
            for note in notes {
                db.note(note);
            }
            db.emit();
        }
    }
    pub fn bug(&self, msg: &str) -> ! {
        self.parse_sess.span_diagnostic.bug(msg);
    }
    pub fn trace_macros(&self) -> bool {
        self.ecfg.trace_mac
    }
    pub fn set_trace_macros(&mut self, x: bool) {
        self.ecfg.trace_mac = x
    }
    pub fn ident_of(&self, st: &str) -> ast::Ident {
        ast::Ident::from_str(st)
    }
    pub fn std_path(&self, components: &[&str]) -> Vec<ast::Ident> {
        let mut v = Vec::new();
        if let Some(s) = self.crate_root {
            v.push(self.ident_of(s));
        }
        v.extend(components.iter().map(|s| self.ident_of(s)));
        v
    }
    pub fn name_of(&self, st: &str) -> ast::Name {
        Symbol::intern(st)
    }

    pub fn check_unused_macros(&self) {
        self.resolver.check_unused_macros();
    }
}

/// Extract a string literal from the macro expanded version of `expr`,
/// emitting `err_msg` if `expr` is not a string literal. This does not stop
/// compilation on error, merely emits a non-fatal error and returns None.
pub fn expr_to_spanned_string(
    cx: &mut ExtCtxt,
    expr: P<ast::Expr>,
    err_msg: &str,
) -> Option<Spanned<(Symbol, ast::StrStyle)>> {
    // Update `expr.span`'s ctxt now in case expr is an `include!` macro invocation.
    let expr = expr.map(|mut expr| {
        expr.span.ctxt = expr.span.ctxt.apply_mark(cx.current_expansion.mark);
        expr
    });

    // we want to be able to handle e.g. concat("foo", "bar")
    let expr = cx.expander().fold_expr(expr);
    match expr.node {
        ast::ExprKind::Lit(ref l) => match l.node {
            ast::LitKind::Str(s, style) => return Some(respan(expr.span, (s, style))),
            _ => cx.span_err(l.span, err_msg),
        },
        _ => cx.span_err(expr.span, err_msg),
    }
    None
}

pub fn expr_to_string(
    cx: &mut ExtCtxt,
    expr: P<ast::Expr>,
    err_msg: &str,
) -> Option<(Symbol, ast::StrStyle)> {
    expr_to_spanned_string(cx, expr, err_msg).map(|s| s.node)
}

/// Non-fatally assert that `tts` is empty. Note that this function
/// returns even when `tts` is non-empty, macros that *need* to stop
/// compilation should call
/// `cx.parse_sess.span_diagnostic.abort_if_errors()` (this should be
/// done as rarely as possible).
pub fn check_zero_tts(cx: &ExtCtxt, sp: Span, tts: &[tokenstream::TokenTree], name: &str) {
    if !tts.is_empty() {
        cx.span_err(sp, &format!("{} takes no arguments", name));
    }
}

/// Extract the string literal from the first token of `tts`. If this
/// is not a string literal, emit an error and return None.
pub fn get_single_str_from_tts(
    cx: &mut ExtCtxt,
    sp: Span,
    tts: &[tokenstream::TokenTree],
    name: &str,
) -> Option<String> {
    let mut p = cx.new_parser_from_tts(tts);
    if p.token == token::Eof {
        cx.span_err(sp, &format!("{} takes 1 argument", name));
        return None;
    }
    let ret = panictry!(p.parse_expr());
    if p.token != token::Eof {
        cx.span_err(sp, &format!("{} takes 1 argument", name));
    }
    expr_to_string(cx, ret, "argument must be a string literal").map(|(s, _)| s.to_string())
}

/// Extract comma-separated expressions from `tts`. If there is a
/// parsing error, emit a non-fatal error and return None.
pub fn get_exprs_from_tts(
    cx: &mut ExtCtxt,
    sp: Span,
    tts: &[tokenstream::TokenTree],
) -> Option<Vec<P<ast::Expr>>> {
    let mut p = cx.new_parser_from_tts(tts);
    let mut es = Vec::new();
    while p.token != token::Eof {
        es.push(cx.expander().fold_expr(panictry!(p.parse_expr())));
        if p.eat(&token::Comma) {
            continue;
        }
        if p.token != token::Eof {
            cx.span_err(sp, "expected token: `,`");
            return None;
        }
    }
    Some(es)
}

pub struct ChangeSpan {
    pub span: Span,
}

impl Folder for ChangeSpan {
    fn new_span(&mut self, _sp: Span) -> Span {
        self.span
    }

    fn fold_mac(&mut self, mac: ast::Mac) -> ast::Mac {
        fold::noop_fold_mac(mac, self)
    }
}
