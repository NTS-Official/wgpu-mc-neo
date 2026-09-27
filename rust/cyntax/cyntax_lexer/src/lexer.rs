use cyntax_common::ast::PreprocessingToken;
use cyntax_common::ast::Punctuator;
use cyntax_common::ast::Whitespace;
use cyntax_common::ctx::HasContext;
use cyntax_common::ctx::HasMutContext;
use cyntax_common::ctx::ParseContext;
use cyntax_common::span;
use cyntax_common::spanned::Location;
use cyntax_common::spanned::Spanned;
use peekmore::PeekMore;
use peekmore::PeekMoreIterator;

use crate::prelexer::PrelexerIter;
use std::ops::Range;

// Identifier safe characters
#[macro_export]
macro_rules! identifier {
    () => {
        nondigit!() | digit!()
    };
}
#[macro_export]
macro_rules! nondigit {
    () => {
        '_' | 'A'..='Z' | 'a'..='z'
    };
}
#[macro_export]
macro_rules! digit {
    () => {
        '0'..='9'
    };
}
#[macro_export]
macro_rules! opening_delimiter {
    () => {
        '(' | '{' | '['
    };
}

pub use digit;
pub use identifier;
pub use nondigit;
pub use opening_delimiter;
/// Characters can span multiple characters, so a simple `usize` index is not enough.
/// an extreme example is a trigraph. the source `??=`, is lexed as only 1 character, that spans from 0..2
pub type CharLocation = Range<usize>;

#[derive(Debug)]
pub struct Lexer<'src> {
    pub ctx: &'src mut ParseContext,
    pub chars: PeekMoreIterator<PrelexerIter<'src>>,
    at_start_of_line: bool,
    inside_control_line: bool,
}

impl<'src> Iterator for Lexer<'src> {
    type Item = Spanned<PreprocessingToken>;

    fn next(&mut self) -> Option<Self::Item> {
        let next = self.chars.next()?;
        let is_newline = matches!(next.value, '\n');

        let token = match next {
            first_character @ span!(nondigit!()) => {
                let identifier = self.lex_identifier(&first_character);
                Some(identifier.map(|identifier| PreprocessingToken::Identifier(identifier)))
            }

            // Literals, technically we should've parse string literals (or escaped char literals) but it hasn't been a problem yet
            span!(range, '"') => {
                let string = self.lex_string_literal(range);
                // Some(string.map(|string| PreprocessingToken::StringLiteral(self.ctx.strings.get_or_intern(string))))
                Some(string.map(|string| PreprocessingToken::StringLiteral(string)))
            }
            span!(range, '\'') => {
                let string = self.lex_char_literal(range);
                Some(string.map(|string| PreprocessingToken::CharLiteral(string)))
            }
            first_character @ span!(digit!()) => {
                let number = self.lex_number(&first_character);
                Some(number.map(|number| PreprocessingToken::PPNumber(number)))
            }
            // Digits can start with .
            first_character @ span!('.') if matches!(self.chars.peek(), Some(span!(digit!()))) => {
                let number = self.lex_number(&first_character);
                Some(number.map(|number| PreprocessingToken::PPNumber(number)))
            }
            // span!(range, c@ ('(' | ')' | '{' | '}' | '[' | ']')) if self.ignore_delimiters => Some(
            //     Spanned::new(range, Token::Punctuator(Punctuator::from_char(c).unwrap())),
            // ),
            // Entirely skip over line comments
            span!('/') if matches!(self.chars.peek(), Some(span!('/'))) => {
                while let Some(span!(char_span, char)) = self.chars.next() {
                    if char == '\n' {
                        return Some(Spanned::new(
                            char_span,
                            PreprocessingToken::Whitespace(Whitespace::Newline),
                        ));
                    }
                }
                self.next()
            }
            // inline comments
            span!('/') if matches!(self.chars.peek(), Some(span!('*'))) => {
                while let Some(span!(char_span, char)) = self.chars.next() {
                    if char == '*' && matches!(self.chars.peek(), Some(span!('/'))) {
                        self.chars.next().unwrap();
                        return Some(Spanned::new(
                            char_span,
                            PreprocessingToken::Whitespace(Whitespace::Newline),
                        ));
                    }
                }
                self.next()
            }
            span!(location, '#') if self.at_start_of_line => {
                let mut tokens = vec![];
                let mut end = location.clone();
                let mut add = true;
                while let Some(span!(token)) = self.chars.peek() {
                    if matches!(token, '\n') {
                        break;
                    } else if matches!(token, '/')
                        && matches!(self.chars.peek_nth(1), Some(span!('/')))
                    {
                        add = false;
                        self.chars.next().unwrap();
                        self.chars.next().unwrap();
                    } else {
                        self.inside_control_line = true;
                        let n = self.next().unwrap();
                        self.inside_control_line = false;
                        if add {
                            end = n.location.clone();
                            tokens.push(n);
                        }
                    }
                }
                Some(Spanned::new(
                    location.until(&end),
                    PreprocessingToken::ControlLine(tokens),
                ))
            }
            span!(location, '<')
                if matches!(self.chars.peek_nth(0), Some(span!('<')))
                    && matches!(self.chars.peek_nth(1), Some(span!('='))) =>
            {
                self.chars.next().unwrap();
                let last_char = self.chars.next().unwrap();
                Some(self.span(
                    location.range.start..last_char.end(),
                    PreprocessingToken::Punctuator(Punctuator::LeftLeftEqual),
                ))
            }
            span!(location, '>')
                if matches!(self.chars.peek_nth(0), Some(span!('>')))
                    && matches!(self.chars.peek_nth(1), Some(span!('='))) =>
            {
                self.chars.next().unwrap();
                let last_char = self.chars.next().unwrap();
                Some(self.span(
                    location.range.start..last_char.end(),
                    PreprocessingToken::Punctuator(Punctuator::RightRightEqual),
                ))
            }
            span!(location, '.')
                if matches!(self.chars.peek_nth(0), Some(span!('.')))
                    && matches!(self.chars.peek_nth(1), Some(span!('.'))) =>
            {
                self.chars.next().unwrap();
                let last_dot = self.chars.next().unwrap();
                Some(self.span(
                    location.range.start..last_dot.end(),
                    PreprocessingToken::Punctuator(Punctuator::DotDotDot),
                ))
            }

            span!(range, '#') if matches!(self.chars.peek(), Some(span!('#'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::HashHash),
                ))
            }
            span!(range, '+') if matches!(self.chars.peek(), Some(span!('+'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::PlusPlus),
                ))
            }
            span!(range, '-') if matches!(self.chars.peek(), Some(span!('-'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::MinusMinus),
                ))
            }
            span!(range, '&') if matches!(self.chars.peek(), Some(span!('&'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::AndAnd),
                ))
            }
            span!(range, '=') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::EqualEqual),
                ))
            }
            span!(range, '-') if matches!(self.chars.peek(), Some(span!('>'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::Arrow),
                ))
            }
            span!(range, '<') if matches!(self.chars.peek(), Some(span!('<'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::LeftLeft),
                ))
            }
            span!(range, '<') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::LeftEqual),
                ))
            }
            span!(range, '>') if matches!(self.chars.peek(), Some(span!('>'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::RightRight),
                ))
            }
            span!(range, '>') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::RightEqual),
                ))
            }
            span!(range, '!') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::BangEqual),
                ))
            }
            span!(range, '|') if matches!(self.chars.peek(), Some(span!('|'))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::PipePipe),
                ))
            }
            span!(range, '+') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::PlusEqual),
                ))
            }
            span!(range, '-') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::MinusEqual),
                ))
            }
            span!(range, '*') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::AsteriskEqual),
                ))
            }
            span!(range, '%') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::PercentEqual),
                ))
            }
            span!(range, '/') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::SlashEqual),
                ))
            }
            span!(range, '&') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::AndEqual),
                ))
            }
            span!(range, '|') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::PipeEqual),
                ))
            }
            span!(range, '^') if matches!(self.chars.peek(), Some(span!('='))) => {
                self.chars.next().unwrap();
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::CaretEqual),
                ))
            }
            span!(range, punctuator) if Punctuator::is_punctuation(punctuator) => {
                Some(Spanned::new(
                    range,
                    PreprocessingToken::Punctuator(Punctuator::from_char(punctuator).unwrap()),
                ))
            }
            // return early, so we dont clobber "at_start_of_line"
            span!(range, ' ') => {
                return Some(Spanned::new(
                    range,
                    PreprocessingToken::Whitespace(Whitespace::Space),
                ));
            }
            span!(range, '\t') => {
                return Some(Spanned::new(
                    range,
                    PreprocessingToken::Whitespace(Whitespace::Tab),
                ));
            }
            span!(range, '\n') => Some(Spanned::new(
                range,
                PreprocessingToken::Whitespace(Whitespace::Newline),
            )),
            // "form feed"
            span!(range, '\u{c}') => Some(Spanned::new(
                range,
                PreprocessingToken::Whitespace(Whitespace::Space),
            )),
            // ch => self.unwrap_diagnostic(Err(cyntax_errors::errors::SimpleError(ch.location, format!("unimplemented character {}/{}", ch.value, ch.value.escape_debug())).into_codespan_report())),
            ch => panic!(
                "unimplemented char {}/{}",
                ch.value,
                ch.value.escape_debug()
            ),
        };

        // Set this after lexing the token, otherwise it would always be false for non-newline tokens
        self.at_start_of_line = is_newline;

        token
    }
}
impl<'src> Lexer<'src> {
    pub fn new(ctx: &'src mut ParseContext, source: &'src str) -> Lexer<'src> {
        Lexer {
            chars: PrelexerIter::new(ctx.current_file, source).peekmore(),
            at_start_of_line: true,
            inside_control_line: false,
            ctx,
        }
    }
    fn span<T>(&self, range: Range<usize>, value: T) -> Spanned<T> {
        Spanned::new(
            Location {
                range,
                file_id: self.ctx.current_file,
            },
            value,
        )
    }
    pub fn lex_identifier(&mut self, first_character: &Spanned<char>) -> Spanned<String> {
        // let mut ranges = vec![first_character.start..first_character.end];
        let mut identifier = String::from(first_character.value);
        let mut previous_end = first_character.end();

        while let Some(span!(identifier!())) = self.chars.peek() {
            let next = self.chars.next().unwrap();
            previous_end = next.end();
            identifier.push(next.value);
        }
        self.span(first_character.start()..previous_end, identifier)
    }
    pub fn lex_string_literal(&mut self, location: Location) -> Spanned<String> {
        let mut ranges = String::new();
        let mut end = location.range.end;

        while let Some(span!(c)) = self.chars.peek() {
            if *c == '"' {
                let end_quote = self.chars.next().unwrap();
                end = end_quote.end();
                break;
            }
            // Handle escaped characters within string literal
            if *c == '\\' && matches!(self.chars.peek_nth(1).unwrap(), span!('"' | '\\')) {
                // Skip \
                self.chars.next().unwrap();
            }

            let next = self.chars.next().unwrap();
            end = next.end();
            ranges.push(next.value);
        }

        self.span(location.range.start..end, ranges)
    }
    pub fn lex_char_literal(&mut self, location: Location) -> Spanned<String> {
        let mut ranges = String::new();
        let mut end = location.range.end;

        while let Some(span!(c)) = self.chars.peek() {
            if *c == '\'' {
                let end_quote = self.chars.next().unwrap();
                end = end_quote.end();
                break;
            }
            // Handle escaped characters within string literal
            if *c == '\\' && matches!(self.chars.peek_nth(1).unwrap(), span!('\'' | '\\')) {
                // Skip \
                self.chars.next().unwrap();
            }

            let next = self.chars.next().unwrap();
            end = next.end();
            ranges.push(next.value);
        }

        self.span(location.range.start..end, ranges)
    }
    pub fn lex_number(&mut self, first_character: &Spanned<char>) -> Spanned<String> {
        let start = first_character.start();
        let mut end = first_character.end();
        let mut number = String::from(first_character.value);

        let mut expecting_exponent = false;
        while let Some(span!(c)) = self.chars.peek() {
            match c {
                'e' | 'E' | 'p' | 'P' => {
                    expecting_exponent = true;
                    // e / E / p / P
                    let c = self.chars.next().unwrap();
                    end = c.end();
                    number.push(c.value);
                }
                '+' | '-' if expecting_exponent => {
                    expecting_exponent = false;
                    let sign = self.chars.next().unwrap();
                    end = sign.end();
                    number.push(sign.value);
                }
                '.' | digit!() => {
                    let dot_or_digit = self.chars.next().unwrap();
                    end = dot_or_digit.end();
                    number.push(dot_or_digit.value);
                }

                // Identifier parts must start with a nondigit, otherwise it would just... be a part of the preceeding number
                nondigit!() => {
                    let nondigit = self.chars.next().unwrap();
                    end = nondigit.end();
                    number.push(nondigit.value);
                }
                _ => break,
            }
        }

        self.span(start..end, number)
    }
}
// Util functions
impl<'src> Lexer<'src> {
    // pub fn fatal_diagnostic<E: cyntax_errors::Diagnostic>(&mut self, diagnostic: E) -> ! {
    //     let f = self.ctx.current_file();
    //     cyntax_errors::write_codespan_report(diagnostic.into_codespan_report(), &self.ctx.current_file().name, file_source)
    //     panic!("{}", )
    // }
    pub fn ignore_preceeding_whitespace<T, F>(&mut self, mut f: F) -> T
    where
        F: FnMut(&mut Self) -> T,
    {
        while let Some(span!(' ' | '\t')) = self.chars.peek() {
            self.chars.next().unwrap();
        }
        f(self)
    }
}
impl<'a> HasContext for Lexer<'a> {
    fn ctx(&self) -> &ParseContext {
        self.ctx
    }
}
impl<'a> HasMutContext for Lexer<'a> {
    fn ctx_mut(&mut self) -> &mut ParseContext {
        self.ctx
    }
}
