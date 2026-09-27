use cyntax_common::ast::{Keyword, Punctuator};
use cyntax_common::span;
use cyntax_common::spanned::Spanned;
use cyntax_errors::errors::SimpleWarning;
use cyntax_errors::{Diagnostic, errors::SimpleError};

use crate::ast::ParameterList;
use crate::{
    PResult, Parser,
    ast::{
        self, EnumDeclaration, EnumSpecifier, ParameterDeclaration, Pointer, SpecifierQualifier,
        StructDeclarator, StructOrUnionDeclaration, StructOrUnionSpecifier, Token, TypeQualifier,
        TypeSpecifier,
    },
};

impl<'src> Parser<'src> {
    pub fn parse_struct_or_union_type_specifier(
        &mut self,
        is_union: bool,
    ) -> PResult<ast::TypeSpecifier> {
        let tag = if let span!(span, Token::Identifier(identifer)) = self.peek_token()? {
            Some(span.to_spanned(identifer.clone()))
        } else {
            None
        };
        if tag.is_some() {
            self.next_token()?;
        }

        if self.eat_if_next(Token::Punctuator(Punctuator::LeftBrace))? {
            let declarations = self.parse_struct_declaration_list()?;

            self.expect_token(
                Token::Punctuator(Punctuator::RightBrace),
                "to close struct type specifier",
            )?;
            if is_union {
                Ok(ast::TypeSpecifier::Union(StructOrUnionSpecifier {
                    tag,
                    declarations: Some(declarations),
                }))
            } else {
                Ok(ast::TypeSpecifier::Struct(StructOrUnionSpecifier {
                    tag,
                    declarations: Some(declarations),
                }))
            }
        } else {
            if is_union {
                Ok(ast::TypeSpecifier::Union(StructOrUnionSpecifier {
                    tag,
                    declarations: None,
                }))
            } else {
                Ok(ast::TypeSpecifier::Struct(StructOrUnionSpecifier {
                    tag,
                    declarations: None,
                }))
            }
        }
    }

    pub fn parse_struct_declaration_list(
        &mut self,
    ) -> PResult<Vec<Spanned<StructOrUnionDeclaration>>> {
        let mut struct_declarations = vec![];
        // because i use recoverable parsing on the inside of this, its fine to not check the current token
        while !matches!(
            self.peek_token(),
            Ok(span!(Token::Punctuator(Punctuator::RightBrace)))
        ) {
            let start = self.last_location.clone();
            if let Some((specifier_qualifiers, declarators, span)) = self.maybe_recover(
                |this| {
                    let specifier_qualifiers = this.parse_specifier_qualifier_list()?;
                    if !specifier_qualifiers
                        .iter()
                        .any(|sq| matches!(sq, span!(span, SpecifierQualifier::Specifier(_))))
                    {
                        return Err(SimpleError(
                            this.last_location.clone(),
                            "Struct declarations must have atleast one specifier".to_string(),
                        )
                        .into_codespan_report());
                    }
                    let declarators = this.parse_struct_declarator_list()?;
                    let end = this.expect_token(
                        Token::Punctuator(Punctuator::Semicolon),
                        "to end a struct declaration",
                    )?;

                    Ok(Some((
                        specifier_qualifiers,
                        declarators,
                        start.until(&end.location),
                    )))
                },
                |_| None,
                Token::Punctuator(Punctuator::Semicolon),
            ) {
                struct_declarations.push(span.into_spanned(StructOrUnionDeclaration {
                    declarators,
                    specifier_qualifiers,
                }));
            }
        }

        Ok(struct_declarations)
    }
    pub fn parse_enum_type_specifier(&mut self) -> PResult<ast::TypeSpecifier> {
        let name = if let span!(Token::Identifier(identifer)) = self.peek_token()? {
            Some(identifer.clone())
        } else {
            None
        };
        if name.is_some() {
            self.next_token()?;
        }
        if self.eat_if_next(Token::Punctuator(Punctuator::LeftBrace))? {
            let declarations = self.parse_enum_declaration_list()?;

            // if let Some(span!(loc, _)) = self.eat_next(Token::Punctuator(Punctuator::Comma))? {
            //     self.next_token().unwrap();
            //     self.diagnostics.push(SimpleWarning(loc, "trailing comma".to_string()).into_codespan_report());
            // }
            Ok(ast::TypeSpecifier::Enum(EnumSpecifier {
                identifier: name,
                declarations,
            }))
        } else {
            Ok(ast::TypeSpecifier::Enum(EnumSpecifier {
                identifier: name,
                declarations: vec![],
            }))
        }
    }
    pub fn parse_enum_declaration_list(&mut self) -> PResult<Vec<EnumDeclaration>> {
        let mut enum_declarations = vec![];
        while !self.eat_if_next(Token::Punctuator(Punctuator::RightBrace))? {
            if enum_declarations.len() > 0 {
                let comma = self.expect_token(
                    Token::Punctuator(Punctuator::Comma),
                    "expected comma between enum declarations",
                )?;

                // recover from trailing comma
                if self.eat_if_next(Token::Punctuator(Punctuator::RightBrace))? {
                    self.diagnostics.push(
                        SimpleWarning(comma.location, "trailing comma".to_string())
                            .into_codespan_report(),
                    );
                    break;
                }
            }
            let identifier = self.expect_non_typename_identifier()?;
            let expression = if self.eat_if_next(Token::Punctuator(Punctuator::Equal))? {
                Some(self.parse_expression()?)
            } else {
                None
            };

            enum_declarations.push(EnumDeclaration {
                identifier: identifier,
                value: expression,
            })
        }
        Ok(enum_declarations)
    }
    pub fn parse_specifier_qualifier_list(&mut self) -> PResult<Vec<Spanned<SpecifierQualifier>>> {
        let mut specifier_qualifiers = vec![];
        while self.can_parse_type_qualifier() || self.can_parse_type_specifier() {
            specifier_qualifiers.push(match self.next_token()? {
                span!(span, Token::Keyword(Keyword::Void)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Void))
                }
                span!(span, Token::Keyword(Keyword::Char)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Char))
                }
                span!(span, Token::Keyword(Keyword::Short)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Short))
                }
                span!(span, Token::Keyword(Keyword::Int)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Int))
                }
                span!(span, Token::Keyword(Keyword::Long)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Long))
                }
                span!(span, Token::Keyword(Keyword::Float)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Float))
                }
                span!(span, Token::Keyword(Keyword::Double)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Double))
                }
                span!(span, Token::Keyword(Keyword::Signed)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Signed))
                }
                span!(span, Token::Keyword(Keyword::Unsigned)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Unsigned))
                }
                span!(span, Token::Keyword(Keyword::Bool)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Bool))
                }
                span!(span, Token::Keyword(Keyword::Complex)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::Complex))
                }
                span!(span, Token::Keyword(Keyword::Struct)) => {
                    span.into_spanned(SpecifierQualifier::Specifier(
                        self.parse_struct_or_union_type_specifier(false)?,
                    ))
                }
                span!(span, Token::Keyword(Keyword::Union)) => span.into_spanned(
                    SpecifierQualifier::Specifier(self.parse_struct_or_union_type_specifier(true)?),
                ),

                span!(span, Token::Keyword(Keyword::Enum)) => span.into_spanned(
                    SpecifierQualifier::Specifier(self.parse_enum_type_specifier()?),
                ),

                span!(span, Token::Identifier(identifier)) if self.is_typedef(&identifier) => span
                    .into_spanned(SpecifierQualifier::Specifier(TypeSpecifier::TypedefName(
                        identifier,
                    ))),
                span!(span, Token::Keyword(kw @ type_qualifier!())) => {
                    span.into_spanned(SpecifierQualifier::Qualifier(kw.into()))
                }
                _ => unreachable!(),
            });
        }
        Ok(specifier_qualifiers)
    }
    pub fn parse_struct_declarator_list(&mut self) -> PResult<Vec<StructDeclarator>> {
        let mut declarators = vec![];
        while self.can_start_declarator() || self.consider_comma(&declarators)? {
            if declarators.len() > 0 {
                self.expect_token(
                    Token::Punctuator(Punctuator::Comma),
                    "to seperate declarators in struct declaration",
                )?;
            }
            let declarator = self.parse_declarator()?;
            declarators.push(StructDeclarator {
                declarator: Some(declarator),
            });
        }
        Ok(declarators)
    }

    pub fn can_start_pointer(&mut self) -> PResult<bool> {
        Ok(matches!(
            self.peek_token()?,
            span!(Token::Punctuator(Punctuator::Asterisk))
        ))
    }
    pub fn parse_pointer(&mut self) -> PResult<Spanned<Pointer>> {
        let asterisk = self.expect_token(Token::Punctuator(Punctuator::Asterisk), "for pointer")?;
        let type_qualifiers = self.parse_type_qualifiers()?;

        if self.can_start_pointer()? {
            let ptr = self.parse_pointer()?;
            Ok(Spanned::new(
                asterisk.location.until(&ptr.location),
                Pointer {
                    type_qualifiers,
                    ptr: Some(Box::new(ptr)),
                },
            ))
        } else {
            let range = asterisk.location.as_fallback_for_vec(&type_qualifiers);
            Ok(Spanned::new(
                range,
                Pointer {
                    type_qualifiers,
                    ptr: None,
                },
            ))
        }
    }
    pub fn parse_type_qualifiers(&mut self) -> PResult<Vec<Spanned<TypeQualifier>>> {
        let mut type_qualifiers = vec![];

        while self.can_parse_type_qualifier() {
            type_qualifiers.push(self.parse_type_qualifier()?);
        }
        Ok(type_qualifiers)
    }
    pub fn can_parse_type_qualifier(&mut self) -> bool {
        matches!(
            self.peek_token(),
            Ok(span!(Token::Keyword(type_qualifier!())))
        )
    }
    pub fn can_parse_type_specifier(&mut self) -> bool {
        match self.peek_token().cloned() {
            Ok(span!(Token::Keyword(type_specifier!()))) => true,
            Ok(span!(Token::Identifier(identifier))) if self.is_typedef(&identifier) => true,
            _ => false,
        }
    }
    pub fn parse_type_qualifier(&mut self) -> PResult<Spanned<TypeQualifier>> {
        let Spanned { value, location } = self.next_token()?;

        Ok(Spanned::new(
            location,
            match value {
                Token::Keyword(Keyword::Const) => ast::TypeQualifier::Const,
                Token::Keyword(Keyword::Restrict) => ast::TypeQualifier::Restrict,
                Token::Keyword(Keyword::Volatile) => ast::TypeQualifier::Volatile,
                _ => unreachable!(),
            },
        ))
    }

    // pub fn parse_parameter_list(&mut self) -> PResult<Vec<Spanned<ParameterDeclaration>>> {
    //     let mut parameters = vec![];

    //     while self.can_parse_parameter() || self.consider_comma(&parameters)? {
    //         if parameters.len() >= 1 {
    //             self.expect_token(Token::Punctuator(Punctuator::Comma), "to seperate parameters")?;
    //         }
    //         parameters.push(self.parse_parameter_declaration()?)
    //     }
    //     Ok(parameters)
    // }
    pub fn parse_parameter_type_list(&mut self) -> PResult<ParameterList> {
        let mut parameters = vec![];
        let mut is_variadic = false;

        while self.can_parse_parameter() || self.consider_comma(&parameters)? {
            if parameters.len() >= 1 {
                self.expect_token(
                    Token::Punctuator(Punctuator::Comma),
                    "to seperate parameters",
                )?;
            }
            if self.eat_if_next(Token::Punctuator(Punctuator::DotDotDot))? {
                is_variadic = true;
                break;
            } else {
                parameters.push(self.parse_parameter_declaration()?)
            }
        }
        Ok(ParameterList {
            parameters,
            variadic: is_variadic,
        })
    }
    pub fn can_parse_parameter(&mut self) -> bool {
        return self.can_start_declaration_specifier()
            | matches!(
                self.peek_token(),
                Ok(span!(Token::Punctuator(Punctuator::DotDotDot)))
            );
    }
    pub fn parse_parameter_declaration(&mut self) -> PResult<Spanned<ParameterDeclaration>> {
        let start = self.last_location.clone();
        let specifiers = self.parse_declaration_specifiers()?;

        let declarator = if self.can_start_declarator() {
            Some(self.parse_declarator()?)
        } else if self.can_start_abstract_declarator() {
            Some(self.parse_abstract_declarator()?)
        } else {
            None
        };

        // let declarator = if allow_abstract || self.can_start_declarator() {
        //     Some(self.parse_declarator()?)
        // } else {
        //     None
        // };

        if let Some(declarator) = &declarator {
            self.declare_identifier(declarator)?;
        }
        // if declarator.is_none() && !allow_abstract {
        //     return Err(SimpleError(start, "this declaration does not allow abstract declarators".to_string()).into_codespan_report());
        // }

        let range = start.as_fallback_for_vec(&specifiers);
        Ok(Spanned::new(
            range,
            ParameterDeclaration {
                specifiers,
                declarator: declarator,
            },
        ))
    }
}
