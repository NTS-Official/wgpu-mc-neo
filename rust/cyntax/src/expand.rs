use cyntax_common::{
    ast::{Delimited, PreprocessingToken, Punctuator},
    ctx::{HasContext, ParseContext},
    span,
    spanned::{Location, Spanned},
};
use cyntax_consteval::Value;
use cyntax_errors::errors::SimpleError;
use cyntax_errors::{Diagnostic, errors::UnmatchedDelimiter};
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use crate::{
    macros::MacroParameterList,
    prepend::PrependingPeekableIterator,
    substitute::ArgumentSubstitutionIterator,
    tree::{ControlLine, InternalLeaf, IntoTokenTree, TokenTree},
};
pub type ReplacementList = Vec<Spanned<PreprocessingToken>>;
pub type PResult<T> = Result<T, cyntax_errors::codespan_reporting::diagnostic::Diagnostic<usize>>;

#[derive(Debug)]
pub struct Expander<'src, I: Debug + Iterator<Item = TokenTree>> {
    pub ctx: &'src mut ParseContext,
    pub token_trees: PrependingPeekableIterator<I>,
    pub output: Vec<Spanned<PreprocessingToken>>,
    pub macros: HashMap<String, MacroDefinition>,
    pub expanding: HashSet<String>,
    pub respect_defined: bool,
}
#[derive(Debug, Clone)]
pub enum MacroDefinition {
    Object(ReplacementList),
    Function {
        parameter_list: MacroParameterList,
        replacement_list: ReplacementList,
    },
}
pub enum ExpandControlFlow {
    Return(Vec<Spanned<PreprocessingToken>>),
    Rescan(TokenTree),
    RescanMany(Vec<TokenTree>),
}
#[derive(Debug)]
pub struct MacroArgument {
    pub unexpanded: Vec<Spanned<PreprocessingToken>>,
    pub expanded: Vec<Spanned<PreprocessingToken>>,
}
impl<'src, I: Debug + Iterator<Item = TokenTree>> HasContext for Expander<'src, I> {
    fn ctx(&self) -> &ParseContext {
        &self.ctx
    }
}
impl<'src, I: Debug + Iterator<Item = TokenTree>> Expander<'src, I> {
    pub fn new(
        ctx: &'src mut ParseContext,
        token_trees: PrependingPeekableIterator<I>,
        default_macros: HashMap<String, MacroDefinition>,
    ) -> Self {
        // default_macros.insert(ctx.int("__STDC_VERSION__"), MacroDefinition::Object(vec![Spanned::new(Location::new(), PreprocessingToken::PPNumber(ctx.ints("199901L")))]));
        // default_macros.insert(ctx.int("__STDC__"), MacroDefinition::Object(vec![Spanned::new(Location::new(), PreprocessingToken::PPNumber(ctx.ints("1")))]));
        // default_macros.insert(ctx.int("__x86_64__"), MacroDefinition::Object(vec![Spanned::new(Location::new(), PreprocessingToken::PPNumber(ctx.ints("1")))]));
        // default_macros.insert(
        //     ctx.int("__builtin_va_list"),
        //     MacroDefinition::Object(vec![
        //         Spanned::new(Location::new(), PreprocessingToken::Identifier(ctx.ints("void"))),
        //         Spanned::new(Location::new(), PreprocessingToken::Punctuator(Punctuator::Asterisk)),
        //     ]),
        // );
        Self {
            ctx,
            token_trees,
            output: Vec::new(),
            macros: default_macros,
            expanding: HashSet::new(),
            respect_defined: false,
        }
    }
    // this recursion isnt that bad, there shouldnt really ever be many BeginExpandingMacro tokens next to eachother. id be shocked to see more than 10
    pub fn next_token_tree(&mut self) -> Option<TokenTree> {
        match self.token_trees.next() {
            Some(TokenTree::Internal(InternalLeaf::BeginExpandingMacro(mac))) => {
                self.expanding.insert(mac);
                self.next_token_tree()
            }
            Some(TokenTree::Internal(InternalLeaf::FinishExpandingMacro(mac))) => {
                self.expanding.remove(&mac);
                self.next_token_tree()
            }
            Some(tt) => Some(tt),
            None => None,
        }
    }
    pub fn expand(&mut self) -> PResult<()> {
        while let Some(tokens) = self.expand_next(false)? {
            self.output.extend(tokens);
        }
        Ok(())
    }
    pub fn expand_next(
        &mut self,
        skip_macro_replacement: bool,
    ) -> PResult<Option<Vec<Spanned<PreprocessingToken>>>> {
        match self.next_token_tree() {
            Some(tt) => {
                let expanded = self.fully_expand_token_tree(tt, skip_macro_replacement)?;
                Ok(Some(expanded))
            }
            None => Ok(None),
        }
    }
    pub fn fully_expand_token_tree(
        &mut self,
        tt: TokenTree,
        skip_macro_replacement: bool,
    ) -> PResult<Vec<Spanned<PreprocessingToken>>> {
        match self.expand_token_tree(tt, skip_macro_replacement)? {
            ExpandControlFlow::Return(spanneds) => return Ok(spanneds),
            ExpandControlFlow::Rescan(token_tree) => {
                self.token_trees.prepend(token_tree);
                Ok(self.expand_next(skip_macro_replacement)?.unwrap())
            }
            ExpandControlFlow::RescanMany(token_trees) => {
                if token_trees.len() == 0 {
                    return Ok(vec![]);
                } else {
                    // dbg!(&token_trees);
                    self.token_trees.prepend_extend(token_trees.into_iter());
                    let mut result = Vec::new();
                    let tokens = self.expand_next(skip_macro_replacement)?.unwrap();
                    result.extend(tokens);
                    Ok(result)
                }
            }
        }
    }
    pub fn expand_token_tree(
        &mut self,
        tt: TokenTree,
        skip_macro_replacement: bool,
    ) -> PResult<ExpandControlFlow> {
        let mut output = vec![];
        match tt {
            TokenTree::Directive(ControlLine::Error(range, message)) => {
                return Err(
                    cyntax_errors::errors::ErrorDirective(range, message).into_codespan_report()
                );
            }
            TokenTree::Directive(ControlLine::Include(header_name)) => match header_name {
                crate::tree::HeaderName::Q(span!(span, file_name)) => {
                    let content = self.ctx.find_quoted_header(&file_name).ok_or_else(|| {
                        SimpleError(span.clone(), "Could not find quoted header".to_string())
                            .into_codespan_report()
                    })?;
                    // let content = std::fs::read_to_string(self.ctx.strings.resolve(file_name).unwrap()).unwrap();
                    let file = self.ctx.files.add(file_name.to_owned(), content.clone());
                    let current_file = self.ctx.current_file;

                    self.ctx.current_file = file;

                    let lexer = cyntax_lexer::lexer::Lexer::new(self.ctx, &content);
                    let toks = lexer.collect::<Vec<_>>();
                    let trees = IntoTokenTree {
                        ctx: self.ctx,
                        tokens: toks.iter().peekable(),
                        expecting_opposition: false,
                    }
                    .collect::<Vec<TokenTree>>();
                    self.ctx.current_file = current_file;
                    return Ok(ExpandControlFlow::RescanMany(trees));
                }
                crate::tree::HeaderName::H(tokens) => {
                    let first = tokens.first().unwrap();
                    let last = tokens.last().unwrap();
                    let loc = first.location.until(&last.location);
                    let file_name = &self.ctx.files.get(first.location.file_id).unwrap().source()
                        [loc.clone().range];
                    let content = self.ctx.find_bracketed_header(file_name).ok_or_else(|| {
                        SimpleError(loc.clone(), "Could not find bracketed header".to_string())
                            .into_codespan_report()
                    })?;

                    let file = self.ctx.files.add(file_name.to_string(), content.clone());
                    let current_file = self.ctx.current_file;

                    self.ctx.current_file = file;

                    let lexer = cyntax_lexer::lexer::Lexer::new(self.ctx, &content);
                    let toks = lexer.collect::<Vec<_>>();
                    let trees = IntoTokenTree {
                        ctx: self.ctx,
                        tokens: toks.iter().peekable(),
                        expecting_opposition: false,
                    }
                    .collect::<Vec<TokenTree>>();
                    self.ctx.current_file = current_file;

                    return Ok(ExpandControlFlow::RescanMany(trees));
                }
            },
            TokenTree::Directive(control_line) => {
                self.handle_control_line(control_line)?;
            }
            TokenTree::PreprocessorToken(span!(PreprocessingToken::Delimited(delimited))) => {
                let mut v = vec![];
                let inner_tree = delimited
                    .inner_tokens
                    .into_iter()
                    .map(|token| TokenTree::PreprocessorToken(token))
                    .collect::<Vec<_>>();
                v.push(TokenTree::PreprocessorToken(delimited.opener));
                v.extend(inner_tree);
                v.push(TokenTree::PreprocessorToken(delimited.closer));
                return Ok(ExpandControlFlow::RescanMany(v));
            }
            TokenTree::Internal(InternalLeaf::MacroExpansion(macro_name, tt)) => {
                let as_tokens = tt
                    .into_iter()
                    .map(|token| TokenTree::PreprocessorToken(token))
                    .collect::<Vec<_>>();
                let mut f = vec![];
                f.push(TokenTree::Internal(InternalLeaf::BeginExpandingMacro(
                    macro_name.clone(),
                )));
                f.extend(as_tokens);
                f.push(TokenTree::Internal(InternalLeaf::FinishExpandingMacro(
                    macro_name.clone(),
                )));
                return Ok(ExpandControlFlow::RescanMany(f));
            }
            TokenTree::PreprocessorToken(
                span!(defined_span, PreprocessingToken::Identifier(identifier)),
            ) if self.respect_defined && identifier == "defined" => {
                let zero = "0";
                let one = "1";

                match self.peek_non_whitespace() {
                    Some(TokenTree::PreprocessorToken(span!(PreprocessingToken::Identifier(ident)))) => {
                        let _ = self.next_non_whitespace().unwrap();
                        let result = if self.macros.get(&ident).is_some() { one } else { zero };
                        return Ok(ExpandControlFlow::Return(vec![Spanned::new(defined_span.clone(), PreprocessingToken::PPNumber(result.to_string()))]));
                    }
                    Some(TokenTree::PreprocessorToken(span!(PreprocessingToken::Punctuator(Punctuator::LeftParen)))) => {
                        let inner = self.next_delimited();
                        if inner.2.len() != 1 {
                            return Err(SimpleError(inner.0.location.until(&inner.1.location), "defined(..) operator must have exactly 1 identifier inside.".to_string()).into_codespan_report());
                        }
                        assert_eq!(inner.2.len(), 1);
                        if let Some(span!(PreprocessingToken::Identifier(ident))) = &inner.2.get(0) {
                            let result = if self.macros.get(ident).is_some() { one } else { zero };
                            return Ok(ExpandControlFlow::Return(vec![Spanned::new(defined_span.clone(), PreprocessingToken::PPNumber(result.to_string()))]));
                        } else {
                            return Err(SimpleError(inner.0.location.until(&inner.1.location), "Token directly following `defined(` must be an identifier, with a following `)`.".to_string()).into_codespan_report());
                        }
                    }
                    Some(TokenTree::PreprocessorToken(span!(span, _token))) => return Err(SimpleError(span.clone(), "Token directly following `defined` must be an identifier, optionally wrapped in parenthesis".to_string()).into_codespan_report()),
                    _ => panic!("WHAT?"),
                }
            }
            TokenTree::PreprocessorToken(
                span!(span, PreprocessingToken::Identifier(identifier)),
            ) if self.expanding.contains(&identifier) => {
                return Ok(ExpandControlFlow::Return(vec![Spanned::new(
                    span.clone(),
                    PreprocessingToken::BlueIdentifier(identifier.clone()),
                )]));
            }
            TokenTree::PreprocessorToken(
                span!(span, PreprocessingToken::Identifier(identifier)),
            ) if !skip_macro_replacement => {
                return self.handle_identifier(&span, &identifier);
            }
            TokenTree::PreprocessorToken(spanned) => {
                output.push(spanned);
            }

            TokenTree::IfDef {
                macro_name,
                body,
                opposition,
            } => {
                if self.macros.contains_key(&macro_name) {
                    return Ok(ExpandControlFlow::RescanMany(body));
                } else {
                    return Ok(ExpandControlFlow::Rescan(*opposition));
                }
            }
            TokenTree::IfNDef {
                macro_name,
                body,
                opposition,
            } => {
                if !self.macros.contains_key(&macro_name) {
                    return Ok(ExpandControlFlow::RescanMany(body));
                } else {
                    return Ok(ExpandControlFlow::Rescan(*opposition));
                }
            }

            TokenTree::Elif {
                condition,
                body,
                opposition,
            }
            | TokenTree::If {
                condition,
                body,
                opposition,
            } => {
                let mut expanded_condition = vec![];

                // have to collect here so the reference to self.ctx is dropped
                // maybe there is a more efficient way to do this? copying the context would be a bad idea, since the string interners could get out of sync
                let itt = IntoTokenTree {
                    ctx: self.ctx,
                    tokens: condition.iter().peekable(),
                    expecting_opposition: false,
                }
                .collect::<Vec<_>>();

                let mut expander = Expander::new(
                    self.ctx,
                    PrependingPeekableIterator::new(itt.into_iter()),
                    HashMap::new(),
                );
                expander.macros = self.macros.clone();
                expander.respect_defined = true;
                expander.expand()?;

                expanded_condition.extend(expander.output);

                let mut parser = cyntax_parser::Parser::new(self.ctx, expanded_condition);
                let condition = parser.parse_expression()?;
                let eval =
                    cyntax_consteval::ConstantEvalutator::new(self.ctx).evaluate(&condition)?;

                if let Value::Int(1) = eval {
                    return Ok(ExpandControlFlow::RescanMany(body));
                } else {
                    return Ok(ExpandControlFlow::Rescan(*opposition));
                }
            }
            TokenTree::Else { body, opposition } => {
                // todo: deal with `else if`, its terrible but needs to be supported
                return Ok(ExpandControlFlow::RescanMany(body));
            }
            TokenTree::Endif => {}
            _ => {
                println!("warning: unhandled tt {:#?}", tt)
            }
        }
        Ok(ExpandControlFlow::Return(output))
        // Ok(output)
    }
    pub fn handle_identifier(
        &mut self,
        span: &Location,
        identifier: &str,
    ) -> PResult<ExpandControlFlow> {
        match self.macros.get(identifier).cloned() {
            Some(MacroDefinition::Object(replacement_list)) => {
                let output = ArgumentSubstitutionIterator {
                    ctx: self.ctx,
                    replacements: PrependingPeekableIterator::new(
                        replacement_list
                            .into_iter()
                            .filter(|a| !matches!(a, span!(PreprocessingToken::Whitespace(_)))),
                    ),
                    map: HashMap::new(),
                    variadic_args: vec![],
                    is_variadic: false,
                    glue_next_token: false,
                    glue_string: String::new(),
                    stringify_next_token: false,
                    stringify_string: String::new(),
                }
                .flatten()
                .map(|mut token| {
                    token.location = span.clone();
                    token
                })
                .collect::<Vec<_>>();
                return Ok(ExpandControlFlow::Rescan(TokenTree::Internal(
                    InternalLeaf::MacroExpansion(identifier.to_owned(), output),
                )));
            }
            Some(MacroDefinition::Function {
                parameter_list,
                replacement_list,
            }) => {
                let nws = self.peek_non_whitespace();
                let is_lp = matches!(
                    nws,
                    Some(TokenTree::PreprocessorToken(span!(
                        PreprocessingToken::Punctuator(Punctuator::LeftParen)
                    )))
                );
                let is_lp_delim = /* !is_lp && */  if let Some(TokenTree::PreprocessorToken(span!(PreprocessingToken::Delimited(d)))) = nws {
                    matches!(d.opener, span!(PreprocessingToken::Punctuator(Punctuator::LeftParen)))
                } else {
                    false
                };

                if is_lp || is_lp_delim {
                    let (opener, closer, tokens) = self.next_delimited();
                    let arguments = self.split_delimited(tokens.iter());
                    if (arguments.len() < parameter_list.parameters.len())
                        || (!parameter_list.variadic
                            && (arguments.len() > parameter_list.parameters.len()))
                    {
                        return Err(SimpleError(
                            opener.location.until(&closer.location),
                            format!(
                                "Expected {} parameters, found {}",
                                parameter_list.parameters.len(),
                                arguments.len()
                            ),
                        )
                        .into_codespan_report());
                    }

                    let mut expanded_args = vec![];

                    for arg in arguments {
                        let i = arg.value.clone().into_iter();
                        expanded_args.push(MacroArgument {
                            unexpanded: arg.value.into_iter().cloned().collect(),
                            expanded: self.expand_arg(i.into_iter()),
                        });
                    }
                    let mut eai = expanded_args.into_iter();
                    let map = parameter_list
                        .parameters
                        .into_iter()
                        .zip(eai.by_ref())
                        .collect::<HashMap<_, _>>();

                    let extras = eai.collect::<Vec<_>>();

                    let output = ArgumentSubstitutionIterator {
                        ctx: self.ctx,
                        replacements: PrependingPeekableIterator::new(
                            replacement_list
                                .into_iter()
                                .filter(|a| !matches!(a, span!(PreprocessingToken::Whitespace(_)))),
                        ),
                        map,
                        variadic_args: extras,
                        is_variadic: parameter_list.variadic,
                        glue_next_token: false,
                        glue_string: String::new(),
                        stringify_next_token: false,
                        stringify_string: String::new(),
                    }
                    .flatten()
                    .map(|mut token| {
                        token.location = span.clone();
                        token
                    })
                    .collect::<Vec<_>>();

                    return Ok(ExpandControlFlow::RescanMany(vec![TokenTree::Internal(
                        InternalLeaf::MacroExpansion(identifier.to_owned(), output),
                    )]));
                } else {
                    return Ok(ExpandControlFlow::Return(vec![Spanned::new(
                        span.clone(),
                        PreprocessingToken::Identifier(identifier.to_owned()),
                    )]));
                }
            }
            None => Ok(ExpandControlFlow::Return(vec![Spanned::new(
                span.clone(),
                PreprocessingToken::Identifier(identifier.to_owned()),
            )])),
        }
    }

    pub fn next_delimited(
        &mut self,
    ) -> (
        Spanned<PreprocessingToken>,
        Spanned<PreprocessingToken>,
        Vec<Spanned<PreprocessingToken>>,
    ) {
        let paren = self.next_non_whitespace().unwrap();
        match paren {
            TokenTree::PreprocessorToken(
                ref t @ span!(PreprocessingToken::Punctuator(Punctuator::LeftParen)),
            ) => {
                let (opener, closer, tokens) = self
                    .collect_until_closing_delimiter(t, true)
                    .unwrap()
                    .value
                    .as_delimited();
                (opener, closer, tokens)
            }
            // TokenTree::LexerToken(span!(Token::Delimited { opener, closer, inner_tokens })) => (opener.clone(), closer.clone(), inner_tokens.to_vec()),
            TokenTree::PreprocessorToken(span!(PreprocessingToken::Delimited(d))) => {
                (d.opener.clone(), d.closer.clone(), d.inner_tokens.to_vec())
            }
            t => panic!("ooking for delimited but found {:#?}", t),
        }
    }
    pub fn expand_arg<'a, J: Iterator<Item = &'a Spanned<PreprocessingToken>>>(
        &mut self,
        arg: J,
    ) -> Vec<Spanned<PreprocessingToken>> {
        let mut expander = Expander {
            ctx: self.ctx,
            expanding: self.expanding.clone(),
            macros: self.macros.clone(),
            output: Vec::new(),
            token_trees: PrependingPeekableIterator::new(
                arg.map(|t| TokenTree::PreprocessorToken(t.clone()))
                    .collect::<Vec<_>>()
                    .into_iter(),
            ),
            respect_defined: false,
        };
        expander.expand().unwrap();
        expander.output
    }

    pub fn handle_control_line(&mut self, control_line: ControlLine) -> PResult<()> {
        match control_line {
            ControlLine::DefineFunction {
                macro_name,
                parameters,
                replacement_list,
            } => {
                self.handle_define_function(macro_name, parameters, &replacement_list)?;
            }
            ControlLine::DefineObject {
                macro_name,
                replacement_list,
            } => {
                self.handle_define_object(macro_name, &replacement_list);
            }
            ControlLine::Undefine(macro_name) => {
                self.macros.remove(&macro_name);
            }
            _ => todo!(),
        }
        Ok(())
    }
    pub fn handle_define_function<'func>(
        &mut self,
        macro_name: String,
        parameters: Spanned<PreprocessingToken>,
        replacment_list: &'func Vec<Spanned<PreprocessingToken>>,
    ) -> PResult<()> {
        let parameters = self.parse_parameters(parameters)?;
        self.macros.insert(
            macro_name,
            MacroDefinition::Function {
                parameter_list: parameters.clone(),
                replacement_list: replacment_list.to_vec(),
            },
        );
        Ok(())
    }
    pub fn handle_define_object<'func>(
        &mut self,
        macro_name: String,
        replacment_list: &'func Vec<Spanned<PreprocessingToken>>,
    ) {
        self.macros.insert(
            macro_name,
            MacroDefinition::Object(replacment_list.to_vec()),
        );
    }

    pub fn collect_until_closing_delimiter(
        &mut self,
        opening_token: &Spanned<PreprocessingToken>,
        skip_macro_replacement: bool,
    ) -> PResult<Spanned<PreprocessingToken>> {
        let expected_closer = match opening_token.value {
            PreprocessingToken::Punctuator(Punctuator::LeftParen) => {
                PreprocessingToken::Punctuator(Punctuator::RightParen)
            }
            PreprocessingToken::Punctuator(Punctuator::LeftBracket) => {
                PreprocessingToken::Punctuator(Punctuator::RightBracket)
            }
            PreprocessingToken::Punctuator(Punctuator::LeftBrace) => {
                PreprocessingToken::Punctuator(Punctuator::RightBrace)
            }
            _ => unreachable!(),
        };
        let mut inner = vec![];
        let mut end = opening_token.end();
        while let Some(token) = self.next_token_tree() {
            if !matches!(token, TokenTree::PreprocessorToken(_)) {
                continue;
            }
            let token = token.as_token();
            end = token.end();
            match token {
                span!(
                    rp,
                    ref tok @ PreprocessingToken::Punctuator(
                        Punctuator::RightParen | Punctuator::RightBracket | Punctuator::RightBrace,
                    )
                ) if *tok == expected_closer => {
                    return Ok(Spanned::new(
                        opening_token.location.clone(),
                        PreprocessingToken::Delimited(Box::new(Delimited {
                            opener: opening_token.clone(),
                            closer: Spanned::new(rp.clone(), expected_closer),
                            inner_tokens: inner,
                        })),
                    ));
                }
                span!(token) if token == opening_token.value => {
                    let nested = self
                        .collect_until_closing_delimiter(opening_token, skip_macro_replacement)
                        .unwrap();
                    inner.push(nested);
                }
                token => {
                    inner.push(token.clone());
                }
            }
        }
        Err(UnmatchedDelimiter {
            opening_delimiter_location: opening_token.location.clone(),
            potential_closing_delimiter_location: Location {
                range: end..end,
                file_id: opening_token.location.file_id,
            },
            closing_delimiter: expected_closer,
        }
        .into_codespan_report())
    }
    pub fn peek_non_whitespace(&mut self) -> Option<TokenTree> {
        self.peek_non_whitespace_nth(0)
    }
    pub fn peek_non_whitespace_nth(&mut self, n: usize) -> Option<TokenTree> {
        let tt = self.token_trees.peek_nth(n).cloned();

        match tt {
            Some(TokenTree::Internal(InternalLeaf::BeginExpandingMacro(_))) => {
                self.peek_non_whitespace_nth(n + 1)
            }
            Some(TokenTree::Internal(InternalLeaf::FinishExpandingMacro(_))) => {
                self.peek_non_whitespace_nth(n + 1)
            }
            Some(TokenTree::PreprocessorToken(span!(PreprocessingToken::Whitespace(_)))) => {
                self.peek_non_whitespace_nth(n + 1)
            }
            Some(_) => tt,
            None => None,
        }
    }
    pub fn next_non_whitespace(&mut self) -> Option<TokenTree> {
        let tt = self.next_token_tree()?;

        match tt {
            TokenTree::PreprocessorToken(span!(PreprocessingToken::Whitespace(_))) => {
                self.next_non_whitespace()
            }

            tt => Some(tt),
        }
    }
}
