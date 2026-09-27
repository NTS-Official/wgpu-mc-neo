use std::iter::Peekable;

use cyntax_common::{
    ast::{Delimited, PreprocessingToken, Punctuator, Whitespace},
    ctx::{HasContext, ParseContext},
    span,
    spanned::{Location, Spanned},
};
use cyntax_errors::{Diagnostic, UnwrapDiagnostic};

use crate::{expand::PResult, tree::TokenTree::PreprocessorToken};
#[derive(Debug)]
pub struct IntoTokenTree<'src, I: Iterator<Item = &'src Spanned<PreprocessingToken>>> {
    pub ctx: &'src mut ParseContext,
    pub tokens: Peekable<I>,
    pub expecting_opposition: bool,
}
impl<'src, I: Iterator<Item = &'src Spanned<PreprocessingToken>>> Iterator
    for IntoTokenTree<'src, I>
{
    type Item = TokenTree;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.tokens.next()?;

        match token {
            span!(PreprocessingToken::ControlLine(inner)) => {
                let control_line = self.parse_control_line(inner.to_vec());
                match control_line {
                    ControlLine::DefineObject { .. }
                    | ControlLine::DefineFunction { .. }
                    | ControlLine::Error(..)
                    | ControlLine::Warning(..)
                    | ControlLine::Undefine(..)
                    | ControlLine::Include(..) => {
                        return Some(TokenTree::Directive(control_line));
                    }
                    ControlLine::If { condition } => {
                        let body = self.unwrap_with_diagnostic(|this| this.until_closer(token));
                        let opposition = Box::new(self.maybe_opposition());

                        return Some(TokenTree::If {
                            condition,
                            body,
                            opposition,
                        });
                    }

                    ControlLine::IfDef { macro_name } => {
                        let body = self.unwrap_with_diagnostic(|this| this.until_closer(token));
                        let opposition = Box::new(self.maybe_opposition());
                        return Some(TokenTree::IfDef {
                            macro_name,
                            body,
                            opposition,
                        });
                    }
                    ControlLine::IfNDef { macro_name } => {
                        let body = self.unwrap_with_diagnostic(|this| this.until_closer(token));
                        let opposition = Box::new(self.maybe_opposition());
                        return Some(TokenTree::IfNDef {
                            macro_name,
                            body,
                            opposition,
                        });
                    }
                    ControlLine::Elif { .. } | ControlLine::Else if !self.expecting_opposition => {
                        self.unwrap_diagnostic(Err(cyntax_errors::errors::DanglingEndif(
                            token.location.clone(),
                        )
                        .into_codespan_report()))
                    }
                    ControlLine::Elif { condition } => {
                        let body = self.unwrap_with_diagnostic(|this| this.until_closer(token));
                        let opposition = Box::new(self.maybe_opposition());
                        return Some(TokenTree::Elif {
                            condition,
                            body,
                            opposition,
                        });
                    }
                    ControlLine::Else => {
                        let body = self.unwrap_with_diagnostic(|this| this.until_closer(token));
                        let opposition = Box::new(self.maybe_opposition());

                        return Some(TokenTree::Else { body, opposition });
                    }
                    // Skip these two, they're inert
                    ControlLine::Empty => self.next(),
                    ControlLine::EndIf => self.unwrap_diagnostic(Err(
                        cyntax_errors::errors::DanglingEndif(token.location.clone())
                            .into_codespan_report(),
                    )),
                }
            }
            _ => Some(TokenTree::PreprocessorToken(token.clone())),
        }
    }
}
impl<'src, I: Iterator<Item = &'src Spanned<PreprocessingToken>>> IntoTokenTree<'src, I> {
    pub fn until_closer(
        &mut self,
        opener: &Spanned<PreprocessingToken>,
    ) -> PResult<Vec<TokenTree>> {
        let mut body = vec![];

        while let Some(token) = self.tokens.peek() {
            match token {
                span!(PreprocessingToken::ControlLine(control_line)) => {
                    let inner = self.parse_control_line(control_line.to_vec());
                    match inner {
                        ControlLine::EndIf | ControlLine::Elif { .. } | ControlLine::Else => {
                            return Ok(body);
                        }
                        ControlLine::Empty => {
                            self.tokens.next().unwrap();
                            continue;
                        }
                        _ => {
                            body.push(self.next().unwrap());
                        }
                    }
                }
                _ => body.push(self.next().unwrap()),
            }
        }
        let error = cyntax_errors::errors::UnterminatedTreeNode {
            opening_token: opener.location.clone(),
        };
        Err(error.into_codespan_report())
    }
    pub fn maybe_opposition(&mut self) -> TokenTree {
        match self.tokens.peek() {
            Some(span!(PreprocessingToken::ControlLine(control_line))) => {
                let control_line = self.parse_control_line(control_line.to_vec());
                match control_line {
                    ControlLine::Elif { .. } | ControlLine::Else => {
                        self.expecting_opposition = true;
                        let tree = self.next().unwrap();
                        self.expecting_opposition = false;
                        return tree;
                    }
                    // skip
                    ControlLine::Empty => self.maybe_opposition(),
                    ControlLine::EndIf => {
                        self.tokens.next().unwrap();
                        return TokenTree::Endif;
                    }
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
    }
    // Parse the inside of control line into a usable value
    pub fn parse_control_line(&mut self, tokens: Vec<Spanned<PreprocessingToken>>) -> ControlLine {
        // Handle empty directive
        if tokens.len() == 0 {
            return ControlLine::Empty;
        }

        let mut tokens_iter = tokens.clone().into_iter().peekable();
        // utility function to strip all preceeding whitespace
        let skip_whitespace =
            |tokens_iter: &mut Peekable<std::vec::IntoIter<Spanned<PreprocessingToken>>>| {
                while let Some(token) = tokens_iter.peek() {
                    if matches!(token, span!(PreprocessingToken::Whitespace(_))) {
                        tokens_iter.next().unwrap();
                    } else {
                        break;
                    }
                }
            };

        skip_whitespace(&mut tokens_iter);

        let directive = self
            .expect_identifier(&mut tokens_iter)
            .expect("expected identifier after directive character");
        let directive_name = directive.value;
        let directive_range = directive.location;

        match () {
            _ if directive_name == "ifdef" => {
                skip_whitespace(&mut tokens_iter);

                let macro_name = self
                    .expect_identifier(&mut tokens_iter)
                    .expect("expected macro_name in ifdef directive");

                return ControlLine::IfDef {
                    macro_name: macro_name.value,
                };
            }
            _ if directive_name == "ifndef" => {
                skip_whitespace(&mut tokens_iter);

                let macro_name = self
                    .expect_identifier(&mut tokens_iter)
                    .expect("expected macro_name in ifndef directive");

                return ControlLine::IfNDef {
                    macro_name: macro_name.value,
                };
            }
            _ if directive_name == "else" => {
                return ControlLine::Else;
            }

            _ if directive_name == "elif" => {
                skip_whitespace(&mut tokens_iter);
                let condition = tokens_iter.collect::<Vec<_>>();
                return ControlLine::Elif { condition };
            }
            _ if directive_name == "if" => {
                skip_whitespace(&mut tokens_iter);
                let tokens = tokens_iter.collect();
                return ControlLine::If { condition: tokens };
            }
            _ if directive_name == "endif" => {
                return ControlLine::EndIf;
            }
            _ if directive_name == "define" => {
                skip_whitespace(&mut tokens_iter);

                let macro_name = self
                    .expect_identifier(&mut tokens_iter)
                    .expect("expected macro_name in ifdef directive");
                if matches!(
                    tokens_iter.peek(),
                    Some(span!(PreprocessingToken::Punctuator(Punctuator::LeftParen)))
                ) {
                    let opener = tokens_iter.next().unwrap();
                    let mut parameters = vec![];

                    while let Some(token) = tokens_iter.next() {
                        if matches!(
                            token,
                            span!(PreprocessingToken::Punctuator(Punctuator::RightParen))
                        ) {
                            // let end = parameters.last().map(|param: &Spanned<_>| param.end()).unwrap_or(opener.end());
                            let parameters_token =
                                PreprocessingToken::Delimited(Box::new(Delimited {
                                    opener: opener.map_ref(|_| {
                                        PreprocessingToken::Punctuator(Punctuator::LeftParen)
                                    }),
                                    closer: token.map_ref(|_| {
                                        PreprocessingToken::Punctuator(Punctuator::RightParen)
                                    }),
                                    inner_tokens: parameters,
                                }));
                            skip_whitespace(&mut tokens_iter);
                            let replacement_list = tokens_iter.collect();
                            return ControlLine::DefineFunction {
                                macro_name: macro_name.value,
                                parameters: Spanned::new(opener.location.clone(), parameters_token),
                                replacement_list: replacement_list,
                            };
                        } else {
                            parameters.push(token.clone());
                        }
                    }
                    panic!("todo: error message for unmatched parenthesis in ");
                } else {
                    if matches!(
                        tokens_iter.peek(),
                        Some(span!(PreprocessingToken::Whitespace(Whitespace::Space)))
                    ) {
                        tokens_iter.next().unwrap();
                    }
                    skip_whitespace(&mut tokens_iter);

                    let replacement_list = tokens_iter.collect();
                    return ControlLine::DefineObject {
                        macro_name: macro_name.value,
                        replacement_list,
                    };
                }
            }
            _ if directive_name == "undef" => {
                skip_whitespace(&mut tokens_iter);

                let macro_name = self
                    .expect_identifier(&mut tokens_iter)
                    .expect("expected macro_name in ifdef directive");
                return ControlLine::Undefine(macro_name.value);
            }
            _ if directive_name == "error" => {
                skip_whitespace(&mut tokens_iter);
                let reason = tokens_iter.next();
                return ControlLine::Error(directive_range, reason);
            }
            _ if directive_name == "warning" => {
                let reason = tokens_iter.next();
                return ControlLine::Warning(directive_range, reason);
            }
            // _ if directive_name == self.ctx.strings.get_or_intern_static("include") => {
            //     skip_whitespace(&mut tokens_iter);
            //     match tokens_iter.next() {
            //         Some(span!(PreprocessingToken::Punctuator(Punctuator::Left))) => {
            //             let inner = tokens_iter.take_while(|tok| !matches!(tok, span!(PreprocessingToken::Punctuator(Punctuator::Right)))).collect::<Vec<_>>();
            //             return ControlLine::Include(HeaderName::H(inner));
            //         }
            //         Some(span!(span, PreprocessingToken::StringLiteral(string))) => return ControlLine::Include(HeaderName::Q(span.into_spanned(string))),
            //         // TODO: error
            //         _ => todo!(),
            //     }
            // }
            _ if directive_name == "pragma" => return ControlLine::Empty,
            _ if directive_name == "line" => return ControlLine::Empty,
            _ if directive_name == "version" => return ControlLine::Empty,
            _ => {
                let directive_range = tokens.first().unwrap().start()..tokens.last().unwrap().end();
                let err = cyntax_errors::errors::UnknownDirective(Location {
                    range: directive_range,
                    file_id: self.ctx.current_file,
                });
                let n = directive_name.clone();

                panic!(
                    "unknown directive {:#?} {n} {}",
                    err.into_codespan_report(),
                    n
                )
                // panic!("{}", err.into_codespan_report().with("", self.source));
            }
        };
    }
    pub fn expect_identifier<'b, I2: Iterator<Item = Spanned<PreprocessingToken>>>(
        &mut self,
        iter: &mut I2,
    ) -> Option<Spanned<String>> {
        match iter.next()? {
            span!(range, PreprocessingToken::Identifier(i)) => Some(Spanned::new(range.clone(), i)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ControlLine {
    IfDef {
        macro_name: String,
    },
    IfNDef {
        macro_name: String,
    },

    If {
        condition: Vec<Spanned<PreprocessingToken>>,
    },
    Elif {
        condition: Vec<Spanned<PreprocessingToken>>,
    },
    Else,

    EndIf,
    DefineFunction {
        macro_name: String,
        parameters: Spanned<PreprocessingToken>,
        replacement_list: Vec<Spanned<PreprocessingToken>>,
    },
    DefineObject {
        macro_name: String,
        replacement_list: Vec<Spanned<PreprocessingToken>>,
    },
    Undefine(String),
    Include(HeaderName),
    Error(Location, Option<Spanned<PreprocessingToken>>),
    Warning(Location, Option<Spanned<PreprocessingToken>>),

    Empty,
}
#[derive(Debug, Clone)]
pub enum HeaderName {
    /// "header-name.h"
    Q(Spanned<String>),
    /// <header-name.h>
    H(Vec<Spanned<PreprocessingToken>>),
}
impl<'src, I: Iterator<Item = &'src Spanned<PreprocessingToken>>> HasContext
    for IntoTokenTree<'src, I>
{
    fn ctx(&self) -> &ParseContext {
        &self.ctx
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum TokenTree {
    Directive(ControlLine),
    IfDef {
        macro_name: String,
        body: Vec<TokenTree>,
        opposition: Box<TokenTree>,
    },
    IfNDef {
        macro_name: String,
        body: Vec<TokenTree>,
        opposition: Box<TokenTree>,
    },
    If {
        condition: Vec<Spanned<PreprocessingToken>>,
        body: Vec<TokenTree>,
        opposition: Box<TokenTree>,
    },
    Elif {
        condition: Vec<Spanned<PreprocessingToken>>,
        body: Vec<TokenTree>,
        opposition: Box<TokenTree>,
    },
    Else {
        body: Vec<TokenTree>,
        /// Must be endif?
        opposition: Box<TokenTree>,
    },
    Endif,
    PreprocessorToken(Spanned<PreprocessingToken>),
    Internal(InternalLeaf),
}
#[derive(Debug, Clone)]
pub enum InternalLeaf {
    MacroExpansion(String, Vec<Spanned<PreprocessingToken>>),
    BeginExpandingMacro(String),
    FinishExpandingMacro(String),
}
impl TokenTree {
    pub fn as_token(&self) -> Spanned<PreprocessingToken> {
        match self {
            TokenTree::PreprocessorToken(spanned) => spanned.clone(),
            this => panic!("tried to assume {:?} was a token!", this),
        }
    }
}
