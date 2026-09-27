use crate::{
    expand::{Expander, PResult},
    tree::TokenTree,
};
use cyntax_common::{
    ast::{Delimited, PreprocessingToken, Punctuator},
    span,
    spanned::{Location, Spanned},
};
use cyntax_errors::{Diagnostic, errors::SimpleError};
use std::fmt::Debug;
#[derive(Debug, Clone)]
pub struct MacroParameterList {
    pub parameters: Vec<String>,
    pub variadic: bool,
}
impl<'src, I: Debug + Iterator<Item = TokenTree>> Expander<'src, I> {
    pub fn parse_parameters<'func>(
        &mut self,
        parameter_token: Spanned<PreprocessingToken>,
    ) -> PResult<MacroParameterList> {
        if let span!(PreprocessingToken::Delimited(b)) = parameter_token {
            if let Delimited {
                opener: span!(PreprocessingToken::Punctuator(Punctuator::LeftParen)),
                closer: _closer,
                inner_tokens,
            } = *b
            {
                let no_whitespace = inner_tokens
                    .iter()
                    .filter(|token| !matches!(token, span!(PreprocessingToken::Whitespace(_))));
                let mut parameters = self.split_delimited(no_whitespace);
                let mut is_variadic = false;
                for idx in 0..parameters.len() {
                    macro_rules! p {
                        () => {
                            &parameters[idx]
                        };
                    }
                    if p!().value.len() == 0 {
                        return Err(SimpleError(
                            p!().location.clone(),
                            format!("empty parameter?"),
                        )
                        .into_codespan_report());
                    }
                    // If the `...` is attached to a parameter, remove it and set the parameter list as variadic
                    if idx == parameters.len() - 1 && p!().value.len() == 2 {
                        if matches!(
                            p!().value[1],
                            span!(PreprocessingToken::Punctuator(Punctuator::DotDotDot))
                        ) {
                            is_variadic = true;
                            parameters.get_mut(idx).unwrap().value.pop();
                        }
                    }
                    if p!().value.len() > 1 {
                        return Err(SimpleError(
                            p!().location.clone(),
                            format!("Parameters must be either an identifier, or `...`."),
                        )
                        .into_codespan_report());
                    }
                    if matches!(
                        p!().value.first(),
                        Some(span!(PreprocessingToken::Punctuator(Punctuator::DotDotDot)))
                    ) {
                        //if ... is not the last parameter, raise an error
                        if idx != parameters.len() - 1 {
                            return Err(SimpleError(
                                p!().location.clone(),
                                format!("Variadic arguments must be the final argument."),
                            )
                            .into_codespan_report());
                        }
                        is_variadic = true;
                        parameters.pop();
                    }
                }
                let parameters_as_strings = parameters
                    .into_iter()
                    .map(|argument| match argument.value.first().unwrap() {
                        span!(PreprocessingToken::Identifier(identifier)) => identifier.clone(),
                        // above is a check to make sure that each parameter is exactly one token, and its just an identifier
                        _ => unreachable!(),
                    })
                    .collect();
                Ok(MacroParameterList {
                    parameters: parameters_as_strings,
                    variadic: is_variadic,
                })
            } else {
                Err(SimpleError(
                    parameter_token.location,
                    "internal compiler error: tried to parse parameters but its opener is not ("
                        .to_string(),
                )
                .into_codespan_report())
            }
        } else {
            Err(SimpleError(
                parameter_token.location,
                "internal compiler error: tried to parse parameters but its not a delimited"
                    .to_string(),
            )
            .into_codespan_report())
        }
    }
    /// Splits a comma-delimited sequence of tokens into groups.
    ///
    /// - `[]` -> `[]`
    /// - `[,,]` -> `[[], [], []]`
    /// - `[2+5]` -> `[[2+5]]`
    /// - `[2+5,]` -> `[[2+5], []]`
    pub fn split_delimited<'a, L: Iterator<Item = &'a Spanned<PreprocessingToken>>>(
        &self,
        mut tokens: L,
    ) -> Vec<Spanned<Vec<&'a Spanned<PreprocessingToken>>>> {
        let mut elements = vec![];
        let mut current_element = vec![];

        let mut current_element_end = Location::new();
        let mut current_element_start = Location::new();
        while let Some(token) = tokens.next() {
            if current_element_start == Location::new() {
                current_element_start = token.location.clone();
            }
            match token {
                span!(comma, PreprocessingToken::Punctuator(Punctuator::Comma)) => {
                    elements.push(Spanned::new(
                        current_element_start.until(&current_element_end),
                        std::mem::replace(&mut current_element, Vec::new()),
                    ));
                    current_element_start = comma.clone();
                }
                _ => {
                    current_element_end = token.location.clone();
                    current_element.push(token);
                }
            }
        }
        // todo: figure out correct spans for this, adding like this is a terrible idea
        elements.push(Spanned::new(
            current_element_start.until(&current_element_end),
            std::mem::replace(&mut current_element, Vec::new()),
        ));

        // elements.push(Spanned::new(current_element_start.min(current_element_end) + 1..current_element_end + 1, std::mem::replace(&mut current_element, Vec::new())));

        // manual override for `[]` -> `[]`
        // this can probably be fixed algorithmically
        if elements.len() == 1 && elements.first().unwrap().value.len() == 0 {
            vec![]
        } else {
            elements
        }
    }
}
