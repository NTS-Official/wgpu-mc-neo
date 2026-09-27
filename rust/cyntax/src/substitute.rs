use std::{collections::HashMap, fmt::Debug};

use cyntax_common::{
    ast::{PreprocessingToken, Punctuator},
    ctx::ParseContext,
    span,
    spanned::{Location, Spanned},
};
use cyntax_lexer::lexer::Lexer;

use crate::{expand::MacroArgument, prepend::PrependingPeekableIterator};

pub struct ArgumentSubstitutionIterator<'a, I>
where
    I: Debug + Iterator<Item = Spanned<PreprocessingToken>>,
{
    pub ctx: &'a mut ParseContext,
    pub replacements: PrependingPeekableIterator<I>,
    pub map: HashMap<String, MacroArgument>,
    pub is_variadic: bool,
    pub variadic_args: Vec<MacroArgument>,
    pub glue_next_token: bool,

    pub glue_string: String,

    pub stringify_next_token: bool,
    pub stringify_string: String,
}

impl<'a, I: Debug + Iterator<Item = Spanned<PreprocessingToken>>> Iterator
    for ArgumentSubstitutionIterator<'a, I>
{
    type Item = Vec<Spanned<PreprocessingToken>>;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.replacements.next();
        // dbg!(&token);
        let token = token?;

        match &token {
            token if self.glue_next_token => {
                let a = self.maybe_substitute_arg(token.clone(), false);
                Self::stringify_tokens(a.iter(), &mut self.glue_string);

                if !matches!(
                    self.replacements.peek(),
                    Some(span!(PreprocessingToken::Punctuator(Punctuator::HashHash)))
                ) {
                    let src = format!("{}", self.glue_string);

                    let tokens = Lexer::new(self.ctx, &src)
                        .map(|span| Spanned::new(token.location.clone(), span.value))
                        .collect::<Vec<_>>();

                    self.glue_next_token = false;
                    self.glue_string.clear();
                    Some(tokens)
                } else {
                    self.replacements.next().unwrap();
                    Some(vec![])
                }
            }
            token if self.stringify_next_token => {
                // dbg!(&token);
                let a = self.maybe_substitute_arg(token.clone(), false);
                Self::stringify_tokens(a.iter(), &mut self.stringify_string);
                self.replacements.prepend(Spanned::new(
                    token.location.clone(),
                    PreprocessingToken::StringLiteral(self.stringify_string.clone()),
                ));
                self.stringify_next_token = false;
                self.stringify_string.clear();
                Some(vec![])
            }

            span!(PreprocessingToken::Punctuator(Punctuator::Hash)) => {
                self.stringify_next_token = true;
                Some(vec![])
            }
            token
                if matches!(
                    self.replacements.peek(),
                    Some(span!(PreprocessingToken::Punctuator(Punctuator::HashHash)))
                ) =>
            {
                self.glue_next_token = true;
                let a = self.maybe_substitute_arg(token.clone(), false);
                Self::stringify_tokens(a.iter(), &mut self.glue_string);
                self.replacements.next().unwrap(); // // eat ## 
                Some(vec![])
            }
            span!(PreprocessingToken::Identifier(identifier))
                if self.map.contains_key(identifier) =>
            {
                let expanded = self.map.get(identifier).unwrap().expanded.clone();
                Some(expanded)
            }
            span!(PreprocessingToken::Identifier(identifier))
                if self.is_variadic && *identifier == "__VA_ARGS__" =>
            {
                Some(
                    self.variadic_args
                        .iter()
                        .map(|arg| arg.expanded.clone())
                        .flatten()
                        .intersperse(Spanned::new(
                            Location::new(),
                            PreprocessingToken::Punctuator(Punctuator::Comma),
                        ))
                        .collect::<Vec<_>>(),
                )
            }
            _ => Some(vec![token]),
        }
    }
}

impl<'a, I: Debug + Iterator<Item = Spanned<PreprocessingToken>>>
    ArgumentSubstitutionIterator<'a, I>
{
    pub fn maybe_substitute_arg(
        &mut self,
        token: Spanned<PreprocessingToken>,
        expand: bool,
    ) -> Vec<Spanned<PreprocessingToken>> {
        match token {
            span!(PreprocessingToken::Identifier(identifier))
                if self.map.contains_key(&identifier) =>
            {
                let expanded = if expand {
                    self.map.get(&identifier).unwrap().expanded.clone()
                } else {
                    self.map.get(&identifier).unwrap().unexpanded.clone()
                };
                expanded
            }
            _ => vec![token],
        }
    }
    pub fn stringify_tokens<'b, T: Iterator<Item = &'b Spanned<PreprocessingToken>>>(
        tokens: T,
        s: &mut String,
    ) {
        for token in tokens {
            Self::stringify_token(token, s);
        }
    }
    pub fn stringify_token(token: &Spanned<PreprocessingToken>, s: &mut String) {
        match &token.value {
            PreprocessingToken::Identifier(identifier) => s.push_str(identifier),
            PreprocessingToken::BlueIdentifier(identifier) => s.push_str(identifier),

            PreprocessingToken::StringLiteral(string) => {
                s.push('"');
                s.push_str(string);
                s.push('"');
            }
            PreprocessingToken::CharLiteral(chars) => {
                s.push('\'');
                s.push_str(chars);
                s.push('\'');
            }
            PreprocessingToken::PPNumber(number) => {
                s.push_str(number);
            }
            PreprocessingToken::Whitespace(whitespace) => {
                s.push(match whitespace {
                    cyntax_common::ast::Whitespace::Space => ' ',
                    cyntax_common::ast::Whitespace::Newline => '\n',
                    cyntax_common::ast::Whitespace::Tab => '\t',
                });
            }
            PreprocessingToken::Punctuator(punctuator) => s.push_str(&punctuator.to_string()),
            PreprocessingToken::Delimited(d) => {
                Self::stringify_token(&d.opener, s);
                Self::stringify_tokens(d.inner_tokens.iter(), s);
                Self::stringify_token(&d.closer, s);
            }
            _ => unreachable!(),
        }
    }
}
