use crate::ast;
use crate::attr;
use crate::codemap::respan;
use crate::parse::common::SeqSep;
use crate::parse::parser::{Parser, PathStyle, TokenType};
use crate::parse::token::{self, Nonterminal};
use crate::parse::PResult;
use crate::tokenstream::TokenStream;

use log::debug;

#[derive(PartialEq, Eq, Debug)]
enum InnerAttributeParsePolicy<'a> {
    Permitted,
    NotPermitted { reason: &'a str },
}

const DEFAULT_UNEXPECTED_INNER_ATTR_ERR_MSG: &'static str = "an inner attribute is not \
                                                             permitted in this context";

impl<'a> Parser<'a> {
    /// Parse attributes that appear before an item
    pub fn parse_outer_attributes(&mut self) -> PResult<'a, Vec<ast::Attribute>> {
        let mut attrs: Vec<ast::Attribute> = Vec::new();
        let mut just_parsed_doc_comment = false;
        loop {
            debug!("parse_outer_attributes: self.token={:?}", self.token);
            match self.token {
                token::Pound => {
                    let inner_error_reason = if just_parsed_doc_comment {
                        "an inner attribute is not permitted following an outer doc comment"
                    } else if !attrs.is_empty() {
                        "an inner attribute is not permitted following an outer attribute"
                    } else {
                        DEFAULT_UNEXPECTED_INNER_ATTR_ERR_MSG
                    };
                    let inner_parse_policy = InnerAttributeParsePolicy::NotPermitted {
                        reason: inner_error_reason,
                    };
                    attrs.push(self.parse_attribute_with_inner_parse_policy(inner_parse_policy)?);
                    just_parsed_doc_comment = false;
                }
                token::DocComment(s) => {
                    let attr = attr::mk_sugared_doc_attr(attr::mk_attr_id(), s, self.span);
                    if attr.style != ast::AttrStyle::Outer {
                        let mut err = self.fatal("expected outer doc comment");
                        err.note(
                            "inner doc comments like this (starting with \
                                  `//!` or `/*!`) can only appear before items",
                        );
                        return Err(err);
                    }
                    attrs.push(attr);
                    self.bump();
                    just_parsed_doc_comment = true;
                }
                _ => break,
            }
        }
        Ok(attrs)
    }

    /// Matches `attribute = # ! [ meta_item ]`
    ///
    /// If permit_inner is true, then a leading `!` indicates an inner
    /// attribute
    pub fn parse_attribute(&mut self, permit_inner: bool) -> PResult<'a, ast::Attribute> {
        debug!(
            "parse_attribute: permit_inner={:?} self.token={:?}",
            permit_inner, self.token
        );
        let inner_parse_policy = if permit_inner {
            InnerAttributeParsePolicy::Permitted
        } else {
            InnerAttributeParsePolicy::NotPermitted {
                reason: DEFAULT_UNEXPECTED_INNER_ATTR_ERR_MSG,
            }
        };
        self.parse_attribute_with_inner_parse_policy(inner_parse_policy)
    }

    /// The same as `parse_attribute`, except it takes in an `InnerAttributeParsePolicy`
    /// that prescribes how to handle inner attributes.
    fn parse_attribute_with_inner_parse_policy(
        &mut self,
        inner_parse_policy: InnerAttributeParsePolicy,
    ) -> PResult<'a, ast::Attribute> {
        debug!(
            "parse_attribute_with_inner_parse_policy: inner_parse_policy={:?} self.token={:?}",
            inner_parse_policy, self.token
        );
        let (span, path, tokens, mut style) = match self.token {
            token::Pound => {
                let lo = self.span;
                self.bump();

                if inner_parse_policy == InnerAttributeParsePolicy::Permitted {
                    self.expected_tokens.push(TokenType::Token(token::Not));
                }
                let style = if self.token == token::Not {
                    self.bump();
                    if let InnerAttributeParsePolicy::NotPermitted { reason } = inner_parse_policy {
                        let span = self.span;
                        self.diagnostic()
                            .struct_span_err(span, reason)
                            .note(
                                "inner attributes and doc comments, like `#![no_std]` or \
                                   `//! My crate`, annotate the item enclosing them, and are \
                                   usually found at the beginning of source files. Outer \
                                   attributes and doc comments, like `#[test]` and
                                   `/// My function`, annotate the item following them.",
                            )
                            .emit()
                    }
                    ast::AttrStyle::Inner
                } else {
                    ast::AttrStyle::Outer
                };

                self.expect(&token::OpenDelim(token::Bracket))?;
                let (path, tokens) = self.parse_path_and_tokens()?;
                self.expect(&token::CloseDelim(token::Bracket))?;
                let hi = self.prev_span;

                (lo.to(hi), path, tokens, style)
            }
            _ => {
                let token_str = self.this_token_to_string();
                return Err(self.fatal(&format!("expected `#`, found `{}`", token_str)));
            }
        };

        if inner_parse_policy == InnerAttributeParsePolicy::Permitted && self.token == token::Semi {
            self.bump();
            self.span_warn(
                span,
                "this inner attribute syntax is deprecated. The new syntax is \
                            `#![foo]`, with a bang and no semicolon",
            );
            style = ast::AttrStyle::Inner;
        }

        Ok(ast::Attribute {
            id: attr::mk_attr_id(),
            style: style,
            path: path,
            tokens: tokens,
            is_sugared_doc: false,
            span: span,
        })
    }

    pub fn parse_path_and_tokens(&mut self) -> PResult<'a, (ast::Path, TokenStream)> {
        let meta = match self.token {
            token::Interpolated(ref nt) => match **nt {
                Nonterminal::NtMeta(ref meta) => Some(meta.clone()),
                _ => None,
            },
            _ => None,
        };
        Ok(if let Some(meta) = meta {
            self.bump();
            (
                ast::Path::from_ident(meta.span, ast::Ident::with_empty_ctxt(meta.name)),
                meta.node.tokens(meta.span),
            )
        } else {
            (self.parse_path(PathStyle::Mod)?, self.parse_tokens())
        })
    }

    /// Parse attributes that appear after the opening of an item. These should
    /// be preceded by an exclamation mark, but we accept and warn about one
    /// terminated by a semicolon.

    /// matches inner_attrs*
    pub fn parse_inner_attributes(&mut self) -> PResult<'a, Vec<ast::Attribute>> {
        let mut attrs: Vec<ast::Attribute> = vec![];
        loop {
            match self.token {
                token::Pound => {
                    // Don't even try to parse if it's not an inner attribute.
                    if !self.look_ahead(1, |t| t == &token::Not) {
                        break;
                    }

                    let attr = self.parse_attribute(true)?;
                    assert_eq!(attr.style, ast::AttrStyle::Inner);
                    attrs.push(attr);
                }
                token::DocComment(s) => {
                    // we need to get the position of this token before we bump.
                    let attr = attr::mk_sugared_doc_attr(attr::mk_attr_id(), s, self.span);
                    if attr.style == ast::AttrStyle::Inner {
                        attrs.push(attr);
                        self.bump();
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(attrs)
    }

    fn parse_unsuffixed_lit(&mut self) -> PResult<'a, ast::Lit> {
        let lit = self.parse_lit()?;
        debug!("Checking if {:?} is unusuffixed.", lit);

        if !lit.node.is_unsuffixed() {
            let msg = "suffixed literals are not allowed in attributes";
            self.diagnostic()
                .struct_span_err(lit.span, msg)
                .help(
                    "instead of using a suffixed literal \
                                    (1u8, 1.0f32, etc.), use an unsuffixed version \
                                    (1, 1.0, etc.).",
                )
                .emit()
        }

        Ok(lit)
    }

    /// Per RFC#1559, matches the following grammar:
    ///
    /// meta_item : IDENT ( '=' UNSUFFIXED_LIT | '(' meta_item_inner? ')' )? ;
    /// meta_item_inner : (meta_item | UNSUFFIXED_LIT) (',' meta_item_inner)? ;
    pub fn parse_meta_item(&mut self) -> PResult<'a, ast::MetaItem> {
        let nt_meta = match self.token {
            token::Interpolated(ref nt) => match **nt {
                token::NtMeta(ref e) => Some(e.clone()),
                _ => None,
            },
            _ => None,
        };

        if let Some(meta) = nt_meta {
            self.bump();
            return Ok(meta);
        }

        let lo = self.span;
        let ident = self.parse_ident()?;
        let node = self.parse_meta_item_kind()?;
        Ok(ast::MetaItem {
            name: ident.name,
            node: node,
            span: lo.to(self.prev_span),
        })
    }

    pub fn parse_meta_item_kind(&mut self) -> PResult<'a, ast::MetaItemKind> {
        Ok(if self.eat(&token::Eq) {
            ast::MetaItemKind::NameValue(self.parse_unsuffixed_lit()?)
        } else if self.token == token::OpenDelim(token::Paren) {
            ast::MetaItemKind::List(self.parse_meta_seq()?)
        } else {
            self.eat(&token::OpenDelim(token::Paren));
            ast::MetaItemKind::Word
        })
    }

    /// matches meta_item_inner : (meta_item | UNSUFFIXED_LIT) ;
    fn parse_meta_item_inner(&mut self) -> PResult<'a, ast::NestedMetaItem> {
        let lo = self.span;

        match self.parse_unsuffixed_lit() {
            Ok(lit) => {
                return Ok(respan(
                    lo.to(self.prev_span),
                    ast::NestedMetaItemKind::Literal(lit),
                ))
            }
            Err(ref mut err) => self.diagnostic().cancel(err),
        }

        match self.parse_meta_item() {
            Ok(mi) => {
                return Ok(respan(
                    lo.to(self.prev_span),
                    ast::NestedMetaItemKind::MetaItem(mi),
                ))
            }
            Err(ref mut err) => self.diagnostic().cancel(err),
        }

        let found = self.this_token_to_string();
        let msg = format!("expected unsuffixed literal or identifier, found {}", found);
        Err(self.diagnostic().struct_span_err(lo, &msg))
    }

    /// matches meta_seq = ( COMMASEP(meta_item_inner) )
    fn parse_meta_seq(&mut self) -> PResult<'a, Vec<ast::NestedMetaItem>> {
        self.parse_unspanned_seq(
            &token::OpenDelim(token::Paren),
            &token::CloseDelim(token::Paren),
            SeqSep::trailing_allowed(token::Comma),
            |p: &mut Parser<'a>| p.parse_meta_item_inner(),
        )
    }
}
