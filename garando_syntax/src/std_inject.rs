use crate::ast;
use crate::attr;
use crate::codemap::{self, ExpnInfo, MacroAttribute, NameAndSpan};
use crate::ext::hygiene::{Mark, SyntaxContext};
use crate::ptr::P;
use crate::symbol::{keywords, Symbol};
use crate::syntax_pos::{Span, DUMMY_SP};
use crate::tokenstream::TokenStream;

/// Craft a span that will be ignored by the stability lint's
/// call to codemap's `is_internal` check.
/// The expanded code uses the unstable `#[prelude_import]` attribute.
fn ignored_span(sp: Span) -> Span {
    let mark = Mark::fresh(Mark::root());
    mark.set_expn_info(ExpnInfo {
        call_site: DUMMY_SP,
        callee: NameAndSpan {
            format: MacroAttribute(Symbol::intern("std_inject")),
            span: None,
            allow_internal_unstable: true,
        },
    });
    Span {
        ctxt: SyntaxContext::empty().apply_mark(mark),
        ..sp
    }
}

pub fn injected_crate_name(krate: &ast::Crate) -> Option<&'static str> {
    if attr::contains_name(&krate.attrs, "no_core") {
        None
    } else if attr::contains_name(&krate.attrs, "no_std") {
        Some("core")
    } else {
        Some("std")
    }
}

pub fn maybe_inject_crates_ref(mut krate: ast::Crate, alt_std_name: Option<String>) -> ast::Crate {
    let name = match injected_crate_name(&krate) {
        Some(name) => name,
        None => return krate,
    };

    let crate_name = Symbol::intern(&alt_std_name.unwrap_or_else(|| name.to_string()));

    krate.module.items.insert(
        0,
        P(ast::Item {
            attrs: vec![attr::mk_attr_outer(
                DUMMY_SP,
                attr::mk_attr_id(),
                attr::mk_word_item(Symbol::intern("macro_use")),
            )],
            vis: ast::Visibility::Inherited,
            node: ast::ItemKind::ExternCrate(Some(crate_name)),
            ident: ast::Ident::from_str(name),
            id: ast::DUMMY_NODE_ID,
            span: DUMMY_SP,
        }),
    );

    let span = ignored_span(DUMMY_SP);
    krate.module.items.insert(
        0,
        P(ast::Item {
            attrs: vec![ast::Attribute {
                style: ast::AttrStyle::Outer,
                path: ast::Path::from_ident(span, ast::Ident::from_str("prelude_import")),
                tokens: TokenStream::empty(),
                id: attr::mk_attr_id(),
                is_sugared_doc: false,
                span: span,
            }],
            vis: ast::Visibility::Inherited,
            node: ast::ItemKind::Use(P(codemap::dummy_spanned(ast::ViewPathGlob(ast::Path {
                segments: ["{{root}}", name, "prelude", "v1"]
                    .iter()
                    .map(|name| ast::PathSegment::from_ident(ast::Ident::from_str(name), DUMMY_SP))
                    .collect(),
                span: span,
            })))),
            id: ast::DUMMY_NODE_ID,
            ident: keywords::Invalid.ident(),
            span: span,
        }),
    );

    krate
}
