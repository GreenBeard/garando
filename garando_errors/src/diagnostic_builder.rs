use crate::Diagnostic;
use crate::DiagnosticStyledString;

use crate::syntax_pos::{MultiSpan, Span};
use crate::Handler;
use crate::Level;
use std::fmt::{self, Debug};
use std::ops::{Deref, DerefMut};
use std::thread::panicking;

/// Used for emitting structured error messages and other diagnostic information.
#[must_use]
#[derive(Clone)]
pub struct DiagnosticBuilder<'a> {
    handler: &'a Handler,
    diagnostic: Diagnostic,
}

/// In general, the `DiagnosticBuilder` uses deref to allow access to
/// the fields and methods of the embedded `diagnostic` in a
/// transparent way.  *However,* many of the methods are intended to
/// be used in a chained way, and hence ought to return `self`. In
/// that case, we can't just naively forward to the method on the
/// `diagnostic`, because the return type would be a `&Diagnostic`
/// instead of a `&DiagnosticBuilder<'a>`. This `forward!` macro makes
/// it easy to declare such methods on the builder.
macro_rules! forward {
    // Forward pattern for &self -> &Self
    (pub fn $n:ident(&self, $($name:ident: $ty:ty),*) -> &Self) => {
        pub fn $n(&self, $($name: $ty),*) -> &Self {
            self.diagnostic.$n($($name),*);
            self
        }
    };

    // Forward pattern for &mut self -> &mut Self
    (pub fn $n:ident(&mut self, $($name:ident: $ty:ty),*) -> &mut Self) => {
        pub fn $n(&mut self, $($name: $ty),*) -> &mut Self {
            self.diagnostic.$n($($name),*);
            self
        }
    };

    // Forward pattern for &mut self -> &mut Self, with S: Into<MultiSpan>
    // type parameter. No obvious way to make this more generic.
    (pub fn $n:ident<S: Into<MultiSpan>>(&mut self, $($name:ident: $ty:ty),*) -> &mut Self) => {
        pub fn $n<S: Into<MultiSpan>>(&mut self, $($name: $ty),*) -> &mut Self {
            self.diagnostic.$n($($name),*);
            self
        }
    };
}

impl<'a> Deref for DiagnosticBuilder<'a> {
    type Target = Diagnostic;

    fn deref(&self) -> &Diagnostic {
        &self.diagnostic
    }
}

impl<'a> DerefMut for DiagnosticBuilder<'a> {
    fn deref_mut(&mut self) -> &mut Diagnostic {
        &mut self.diagnostic
    }
}

impl<'a> DiagnosticBuilder<'a> {
    /// Emit the diagnostic.
    pub fn emit(&mut self) {
        if self.cancelled() {
            return;
        }

        match self.level {
            Level::Bug | Level::Fatal | Level::PhaseFatal | Level::Error => {
                self.handler.bump_err_count();
            }

            Level::Warning | Level::Note | Level::Help | Level::Cancelled => {}
        }

        self.handler.emitter.borrow_mut().emit(&self);
        self.cancel();

        if self.level == Level::Error {
            self.handler.panic_if_treat_err_as_bug();
        }

        // if self.is_fatal() {
        //     panic!(FatalError);
        // }
    }

    /// Add a span/label to be included in the resulting snippet.
    /// This is pushed onto the `MultiSpan` that was created when the
    /// diagnostic was first built. If you don't call this function at
    /// all, and you just supplied a `Span` to create the diagnostic,
    /// then the snippet will just include that `Span`, which is
    /// called the primary span.
    pub fn span_label<T: Into<String>>(&mut self, span: Span, label: T) -> &mut Self {
        self.diagnostic.span_label(span, label);
        self
    }

    forward!(pub fn note_expected_found(&mut self,
                                        label: &dyn fmt::Display,
                                        expected: DiagnosticStyledString,
                                        found: DiagnosticStyledString)
                                        -> &mut Self);

    forward!(pub fn note_expected_found_extra(&mut self,
                                              label: &dyn fmt::Display,
                                              expected: DiagnosticStyledString,
                                              found: DiagnosticStyledString,
                                              expected_extra: &dyn fmt::Display,
                                              found_extra: &dyn fmt::Display)
                                              -> &mut Self);

    forward!(pub fn note(&mut self, msg: &str) -> &mut Self);
    forward!(pub fn span_note<S: Into<MultiSpan>>(&mut self,
                                                  sp: S,
                                                  msg: &str)
                                                  -> &mut Self);
    forward!(pub fn warn(&mut self, msg: &str) -> &mut Self);
    forward!(pub fn span_warn<S: Into<MultiSpan>>(&mut self, sp: S, msg: &str) -> &mut Self);
    forward!(pub fn help(&mut self , msg: &str) -> &mut Self);
    forward!(pub fn span_help<S: Into<MultiSpan>>(&mut self,
                                                  sp: S,
                                                  msg: &str)
                                                  -> &mut Self);
    forward!(pub fn span_suggestion(&mut self,
                                    sp: Span,
                                    msg: &str,
                                    suggestion: String)
                                    -> &mut Self);
    forward!(pub fn span_suggestions(&mut self,
                                     sp: Span,
                                     msg: &str,
                                     suggestions: Vec<String>)
                                     -> &mut Self);
    forward!(pub fn set_span<S: Into<MultiSpan>>(&mut self, sp: S) -> &mut Self);
    forward!(pub fn code(&mut self, s: String) -> &mut Self);

    /// Convenience function for internal use, clients should use one of the
    /// struct_* methods on Handler.
    pub fn new(handler: &'a Handler, level: Level, message: &str) -> DiagnosticBuilder<'a> {
        DiagnosticBuilder::new_with_code(handler, level, None, message)
    }

    /// Convenience function for internal use, clients should use one of the
    /// struct_* methods on Handler.
    pub fn new_with_code(
        handler: &'a Handler,
        level: Level,
        code: Option<String>,
        message: &str,
    ) -> DiagnosticBuilder<'a> {
        DiagnosticBuilder {
            handler: handler,
            diagnostic: Diagnostic::new_with_code(level, code, message),
        }
    }

    pub fn into_diagnostic(mut self) -> Diagnostic {
        // annoyingly, the Drop impl means we can't actually move
        let result = self.diagnostic.clone();
        self.cancel();
        result
    }
}

impl<'a> Debug for DiagnosticBuilder<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.diagnostic.fmt(f)
    }
}

/// Destructor bomb - a `DiagnosticBuilder` must be either emitted or cancelled
/// or we emit a bug.
impl<'a> Drop for DiagnosticBuilder<'a> {
    fn drop(&mut self) {
        if !panicking() && !self.cancelled() {
            let mut db = DiagnosticBuilder::new(
                self.handler,
                Level::Bug,
                "Error constructed but not emitted",
            );
            db.emit();
            panic!();
        }
    }
}
