use crate::ast::*;
use constant::{ConstantParser, IntConstant};
use cyntax_common::span;
use cyntax_common::{
    ast::*,
    ctx::ParseContext,
    spanned::{Location, Spanned},
};
use cyntax_errors::{Diagnostic, errors::SimpleError};
use peekmore::PeekMore;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    str::FromStr,
    vec::IntoIter,
};

#[macro_use]
pub mod patterns;
pub mod ast;
pub mod constant;
pub mod decl;
pub mod expr;
pub mod stmt;
pub mod ty;
pub type PResult<T> = Result<T, cyntax_errors::codespan_reporting::diagnostic::Diagnostic<usize>>;

#[derive(Debug)]
struct TokenStream<'src> {
    ctx: &'src mut ParseContext,
    iter: std::vec::IntoIter<Spanned<PreprocessingToken>>,
}
impl<'src> Iterator for TokenStream<'src> {
    type Item = PResult<Spanned<Token>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.iter.next()? {
                span!(span, PreprocessingToken::Identifier(identifier)) => {
                    match Keyword::from_str(&identifier) {
                        Ok(kw) => return Some(Ok(Spanned::new(span, Token::Keyword(kw)))),
                        Err(_) => {
                            return Some(Ok(Spanned::new(span, Token::Identifier(identifier))));
                        }
                    }
                }
                span!(span, PreprocessingToken::BlueIdentifier(identifier)) => {
                    return Some(Ok(Spanned::new(span, Token::Identifier(identifier))));
                }
                span!(span, PreprocessingToken::StringLiteral(string)) => {
                    return Some(Ok(Spanned::new(span, Token::StringLiteral(string))));
                }
                span!(span, PreprocessingToken::CharLiteral(string)) => {
                    return Some(Ok(Spanned::new(span, Token::CharLiteral(string))));
                }
                span!(span, PreprocessingToken::PPNumber(number)) => {
                    return Some(
                        self.parse_pp_number(&span, number)
                            .map(|ic| Spanned::new(span.clone(), Token::Constant(ic))),
                    );
                }

                span!(span, PreprocessingToken::Punctuator(punc)) => {
                    return Some(Ok(Spanned::new(span, Token::Punctuator(punc))));
                }
                span!(PreprocessingToken::Whitespace(_)) => continue,
                x => unreachable!("{:#?}", x), // span!(PreprocessingToken::)
            }
        }
    }
}

impl<'src> TokenStream<'src> {
    pub fn parse_pp_number(&mut self, location: &Location, number: String) -> PResult<IntConstant> {
        ConstantParser::new(location.clone(), &number).lex()
    }
}
#[derive(Debug)]
pub enum IdentifierKind {
    Typename,
    Identifier,
}
#[derive(Debug)]
pub enum ScopeKind {
    Global,
    TranslationUnit,
    Block,
}
#[derive(Debug)]
pub struct Scope {
    pub identifiers: HashMap<Identifier, IdentifierKind>,
    pub kind: ScopeKind,
}
#[derive(Debug)]
pub struct Parser<'src> {
    pub ctx: &'src mut ParseContext,
    pub diagnostics: Vec<cyntax_errors::codespan_reporting::diagnostic::Diagnostic<usize>>,
    token_stream: peekmore::PeekMoreIterator<IntoIter<PResult<Spanned<Token>>>>,
    last_location: Location,
    scopes: Vec<Scope>,
}
impl<'src> Parser<'src> {
    pub fn new(ctx: &'src mut ParseContext, tokens: Vec<Spanned<PreprocessingToken>>) -> Self {
        let t = TokenStream {
            ctx,
            iter: tokens.into_iter(),
        };
        let i = t.collect::<Vec<_>>().into_iter();
        Parser {
            ctx,
            token_stream: i.peekmore(),
            last_location: Location::new(),
            diagnostics: vec![],
            scopes: vec![Scope {
                identifiers: HashMap::new(),
                kind: ScopeKind::Global,
            }],
        }
    }
    pub fn next_token(&mut self) -> PResult<Spanned<Token>> {
        match self.token_stream.next() {
            Some(Ok(token)) => {
                self.last_location = token.location.clone();
                Ok(token)
            }
            Some(Err(e)) => Err(e),
            None => Err(
                SimpleError(self.last_location.clone(), "Unexpected EOF!".to_string())
                    .into_codespan_report(),
            ),
        }
    }
    pub fn peek_token_nth(&mut self, n: usize) -> PResult<&Spanned<Token>> {
        match self.token_stream.peek_nth(n) {
            Some(Ok(token)) => {
                self.last_location = token.location.clone();
                Ok(token)
            }
            Some(Err(e)) => Err(e.clone()),
            None => Err(SimpleError(
                self.last_location.clone(),
                format!(
                    "Unexpected EOF while peeking! {}",
                    std::backtrace::Backtrace::capture()
                ),
            )
            .into_codespan_report()),
            // None => Err(SimpleError(self.last_location.clone(), "Unexpected EOF while peeking!".to_string()).into_codespan_report()),
        }
    }
    pub fn peek_token(&mut self) -> PResult<&Spanned<Token>> {
        self.peek_token_nth(0)
    }

    pub fn eat_if_next(&mut self, expected: Token) -> PResult<bool> {
        match self.peek_token() {
            Ok(span!(next)) if *next == expected => {
                self.next_token().unwrap();
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub fn eat_next(&mut self, expected: Token) -> PResult<Option<Spanned<Token>>> {
        match self.peek_token() {
            Ok(span!(next)) if *next == expected => {
                let next = self.next_token().unwrap();
                Ok(Some(next))
            }
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        }
        // if self.peek_token()?.value == t { Ok(Some(self.next_token().unwrap())) } else { Ok(None) }
    }
    pub fn eat_if_same_variant(&mut self, t: Token) -> PResult<bool> {
        if std::mem::discriminant(&self.peek_token()?.value) == std::mem::discriminant(&t) {
            self.next_token().unwrap();
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub fn peek_matches(&mut self, t: Token) -> PResult<bool> {
        if self.peek_token()?.value == t {
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub fn expect_token(&mut self, t: Token, msg: &str) -> PResult<Spanned<Token>> {
        let location = self.last_location.clone();

        match self.next_token() {
            Ok(stoken) if stoken.value == t => Ok(stoken),
            Ok(span!(span, Token::Identifier(i))) if self.is_typedef(&i) => Err(SimpleError(
                span,
                format!("expected {:?}, found typename {:?}: {msg}", t, i),
            )
            .into_codespan_report()),
            Ok(span!(span, Token::Identifier(i))) if !self.is_typedef(&i) => Err(SimpleError(
                span,
                format!("expected {:?}, found non typename {:?}: {msg}", t, i),
            )
            .into_codespan_report()),
            Ok(stoken) => Err(SimpleError(
                stoken.location,
                format!("expected {:?}, found {:?}: {msg}", t, stoken.value),
            )
            .into_codespan_report()),
            Err(e) => Err(
                SimpleError(location, format!("expected {:?}, found EOF: {:?}", t, e))
                    .into_codespan_report(),
            ),
        }
    }
    pub fn maybe_recover<T: Debug, F: FnMut(&mut Self) -> PResult<T>, E: FnOnce(&Self) -> T>(
        &mut self,
        mut f: F,
        e: E,
        recovery_char: Token,
    ) -> T {
        let v = f(self);
        match v {
            Ok(value) => value,
            Err(err) => {
                self.diagnostics.push(err);
                while let Ok(tok) = self.next_token() {
                    if tok.value == recovery_char {
                        break;
                    }
                }
                e(self)
            }
        }
    }
    pub fn maybe_recover_dont_consume_recovery_char<
        T,
        F: FnMut(&mut Self) -> PResult<T>,
        E: FnMut() -> T,
    >(
        &mut self,
        mut f: F,
        mut e: E,
        recovery_char: Token,
    ) -> T {
        match f(self) {
            Ok(value) => value,
            Err(err) => {
                self.diagnostics.push(err);
                while let Ok(tok) = self.peek_token() {
                    if tok.value == recovery_char {
                        break;
                    } else {
                        self.next_token().unwrap();
                        continue;
                    }
                }
                e()
            }
        }
    }

    pub fn declare_typedef(&mut self, declarator: &Spanned<Declarator>) -> PResult<()> {
        // let declarator_name = self.get_declarator_name(&declarator.value);
        if let Some(declarator_name) = declarator.value.get_identifier() {
            if self.is_object(&declarator_name) {
                return Err(SimpleError(
                    self.last_location.clone(),
                    format!("{} is alerady declared as an object", declarator_name),
                )
                .into_codespan_report());
            }
            self.scopes
                .last_mut()
                .unwrap()
                .identifiers
                .insert(declarator_name.clone(), IdentifierKind::Typename);
            println!("declared {} as typedef", declarator_name);
        }

        Ok(())
    }
    pub fn declare_identifier(&mut self, declarator: &Spanned<Declarator>) -> PResult<()> {
        if let Some(declarator_name) = declarator.value.get_identifier() {
            if self.is_typedef(&declarator_name) {
                return Err(SimpleError(
                    self.last_location.clone(),
                    format!("{} is alerady declared as a typedef", declarator_name),
                )
                .into_codespan_report());
            }
            self.scopes
                .last_mut()
                .unwrap()
                .identifiers
                .insert(declarator_name.clone(), IdentifierKind::Identifier);
            println!("declared {} as identifier", declarator_name);
        }

        Ok(())
    }
    pub fn is_typedef(&self, identifier: &Identifier) -> bool {
        self.scopes.iter().any(|scope| {
            scope
                .identifiers
                .get(identifier)
                .map(|t| matches!(t, IdentifierKind::Typename))
                .unwrap_or(false)
        })
    }
    pub fn is_object(&self, identifier: &Identifier) -> bool {
        self.scopes.iter().any(|scope| {
            scope
                .identifiers
                .get(identifier)
                .map(|t| matches!(t, IdentifierKind::Identifier))
                .unwrap_or(false)
        })
    }
    pub fn is_declared(&self, identifier: &Identifier) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.identifiers.contains_key(identifier))
    }
    pub fn expect_non_typename_identifier(&mut self) -> PResult<Spanned<String>> {
        match self.next_token()? {
            span!(loc, Token::Identifier(identifier)) if !self.is_typedef(&identifier) => {
                Ok(Spanned::new(loc, identifier))
            }
            span!(loc, Token::Identifier(identifier)) if self.is_typedef(&identifier) => {
                Err(SimpleError(
                    loc,
                    format!("expected identifier, found typename {:?}", identifier),
                )
                .into_codespan_report())
            }

            stoken => Err(SimpleError(
                stoken.location,
                format!("expected identifier, found {:?}", stoken.value),
            )
            .into_codespan_report()),
        }
    }
    pub fn consider_comma<T>(&mut self, v: &Vec<T>) -> PResult<bool> {
        Ok(v.len() >= 1
            && matches!(
                self.peek_token(),
                Ok(span!(Token::Punctuator(Punctuator::Comma)))
            ))
    }
    pub fn parse_translation_unit(&mut self) -> PResult<TranslationUnit> {
        self.scoped(ScopeKind::TranslationUnit, |this| {
            let mut external_declarations = vec![];
            while let Some(external_declaration) = this.parse_external_declaration()? {
                external_declarations.push(external_declaration);
            }
            Ok(TranslationUnit {
                external_declarations,
            })
        })
    }
    pub fn push_scope(&mut self, kind: ScopeKind) {
        self.scopes.push(Scope {
            identifiers: HashMap::new(),
            kind,
        });
    }
    pub fn pop_scope(&mut self) {
        self.scopes
            .pop()
            .expect("Tried to pop scope when none existed");
    }
    pub fn scoped<T, F: FnMut(&mut Self) -> T>(&mut self, kind: ScopeKind, mut f: F) -> T {
        self.push_scope(kind);
        let value = f(self);
        self.pop_scope();
        value
    }
}
