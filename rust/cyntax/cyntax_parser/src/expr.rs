use std::path::Prefix;

use crate::Spanned;
use crate::ast::{
    Expression, InfixOperator, Operator, PostfixOperator, PrefixOperator, Token, TypeName,
};
use crate::{PResult, Parser};
use cyntax_common::ast::{Keyword, Punctuator};
use cyntax_common::span;
use cyntax_common::spanned::Location;
use cyntax_errors::{Diagnostic, errors::SimpleError};

impl<'src> Parser<'src> {
    fn prefix_binding_power(operator: &PrefixOperator) -> ((), u8) {
        match operator {
            _ => ((), 33),
        }
    }
    fn infix_binding_power(operator: &InfixOperator) -> Option<(u8, u8)> {
        let bp = match operator {
            InfixOperator::Access | InfixOperator::IndirectAccess => (33, 34),
            InfixOperator::Multiply | InfixOperator::Divide | InfixOperator::Modulo => (31, 32),
            InfixOperator::Add | InfixOperator::Subtract => (29, 30),
            InfixOperator::BitwiseShiftLeft | InfixOperator::BitwiseShiftRight => (27, 28),
            InfixOperator::Less | InfixOperator::LessEqual => (25, 26),
            InfixOperator::Greater | InfixOperator::GreaterEqual => (23, 24),
            InfixOperator::Equal | InfixOperator::NotEqual => (21, 22),
            InfixOperator::BitwiseAnd => (19, 20),
            InfixOperator::BitwiseXor => (17, 18),
            InfixOperator::BitwiseOr => (15, 16),
            InfixOperator::LogicalAnd => (15, 16),
            InfixOperator::LogicalOr => (11, 12),

            // Right associative
            InfixOperator::Assign => (10, 9),
            InfixOperator::AddAssign | InfixOperator::SubtractAssign => (8, 7),
            InfixOperator::MultiplyAssign
            | InfixOperator::DivideAssign
            | InfixOperator::ModuloAssign => (6, 5),
            InfixOperator::BitwiseShiftLeftAssign | InfixOperator::BitwiseShiftRightAssign => {
                (4, 3)
            }
            InfixOperator::BitwiseAndAssign
            | InfixOperator::BitwiseXorAssign
            | InfixOperator::BitwiseOrAssign => (2, 1),
        };

        Some(bp)
    }
    fn postfix_binding_power(operator: &PostfixOperator) -> (u8, ()) {
        match operator {
            PostfixOperator::Ternary => (11, ()),
            _ => (35, ()),
        }
    }

    pub fn as_infix_operator(token: &Spanned<Token>) -> Option<InfixOperator> {
        match token {
            span!(Token::Punctuator(Punctuator::Dot)) => Some(InfixOperator::Access),
            span!(Token::Punctuator(Punctuator::Arrow)) => Some(InfixOperator::IndirectAccess),
            span!(Token::Punctuator(Punctuator::Asterisk)) => Some(InfixOperator::Multiply),
            span!(Token::Punctuator(Punctuator::Slash)) => Some(InfixOperator::Divide),
            span!(Token::Punctuator(Punctuator::Percent)) => Some(InfixOperator::Modulo),
            span!(Token::Punctuator(Punctuator::Plus)) => Some(InfixOperator::Add),
            span!(Token::Punctuator(Punctuator::Minus)) => Some(InfixOperator::Subtract),
            span!(Token::Punctuator(Punctuator::LeftLeft)) => Some(InfixOperator::BitwiseShiftLeft),
            span!(Token::Punctuator(Punctuator::RightRight)) => {
                Some(InfixOperator::BitwiseShiftRight)
            }
            span!(Token::Punctuator(Punctuator::Left)) => Some(InfixOperator::Less),
            span!(Token::Punctuator(Punctuator::LeftEqual)) => Some(InfixOperator::LessEqual),
            span!(Token::Punctuator(Punctuator::Right)) => Some(InfixOperator::Greater),
            span!(Token::Punctuator(Punctuator::RightEqual)) => Some(InfixOperator::GreaterEqual),
            span!(Token::Punctuator(Punctuator::EqualEqual)) => Some(InfixOperator::Equal),
            span!(Token::Punctuator(Punctuator::BangEqual)) => Some(InfixOperator::NotEqual),
            span!(Token::Punctuator(Punctuator::Ampersand)) => Some(InfixOperator::BitwiseAnd),
            span!(Token::Punctuator(Punctuator::Caret)) => Some(InfixOperator::BitwiseXor),
            span!(Token::Punctuator(Punctuator::Pipe)) => Some(InfixOperator::BitwiseOr),
            span!(Token::Punctuator(Punctuator::AndAnd)) => Some(InfixOperator::LogicalAnd),
            span!(Token::Punctuator(Punctuator::PipePipe)) => Some(InfixOperator::LogicalOr),
            span!(Token::Punctuator(Punctuator::Equal)) => Some(InfixOperator::Assign),
            span!(Token::Punctuator(Punctuator::PlusEqual)) => Some(InfixOperator::AddAssign),
            span!(Token::Punctuator(Punctuator::MinusEqual)) => Some(InfixOperator::SubtractAssign),
            span!(Token::Punctuator(Punctuator::AsteriskEqual)) => {
                Some(InfixOperator::MultiplyAssign)
            }
            span!(Token::Punctuator(Punctuator::SlashEqual)) => Some(InfixOperator::DivideAssign),
            span!(Token::Punctuator(Punctuator::PercentEqual)) => Some(InfixOperator::ModuloAssign),
            span!(Token::Punctuator(Punctuator::LeftLeftEqual)) => {
                Some(InfixOperator::BitwiseShiftLeftAssign)
            }
            span!(Token::Punctuator(Punctuator::RightRightEqual)) => {
                Some(InfixOperator::BitwiseShiftRightAssign)
            }
            span!(Token::Punctuator(Punctuator::AndEqual)) => Some(InfixOperator::BitwiseAndAssign),
            span!(Token::Punctuator(Punctuator::CaretEqual)) => {
                Some(InfixOperator::BitwiseXorAssign)
            }
            span!(Token::Punctuator(Punctuator::PipeEqual)) => Some(InfixOperator::BitwiseOrAssign),

            _ => None,
        }
    }
    pub fn as_prefix_operator(token: &Spanned<Token>) -> Option<PrefixOperator> {
        match token {
            span!(Token::Punctuator(Punctuator::Minus)) => Some(PrefixOperator::Minus),
            span!(Token::Punctuator(Punctuator::Plus)) => Some(PrefixOperator::Plus),
            span!(Token::Punctuator(Punctuator::LeftParen)) => Some(PrefixOperator::CastOrParen),
            span!(Token::Punctuator(Punctuator::Tilde)) => Some(PrefixOperator::BitwiseNot),
            span!(Token::Punctuator(Punctuator::Asterisk)) => Some(PrefixOperator::Dereference),
            span!(Token::Punctuator(Punctuator::PlusPlus)) => Some(PrefixOperator::Increment),
            span!(Token::Punctuator(Punctuator::MinusMinus)) => Some(PrefixOperator::Decrement),
            span!(Token::Punctuator(Punctuator::Bang)) => Some(PrefixOperator::LogicalNot),
            span!(Token::Punctuator(Punctuator::Ampersand)) => Some(PrefixOperator::AddressOf),
            span!(Token::Keyword(Keyword::Sizeof)) => Some(PrefixOperator::SizeOf),
            _ => None,
        }
    }
    pub fn as_postfix_operator(token: &Spanned<Token>) -> Option<PostfixOperator> {
        match token {
            span!(Token::Punctuator(Punctuator::PlusPlus)) => Some(PostfixOperator::Increment),
            span!(Token::Punctuator(Punctuator::MinusMinus)) => Some(PostfixOperator::Decrement),
            span!(Token::Punctuator(Punctuator::LeftParen)) => Some(PostfixOperator::Call),
            span!(Token::Punctuator(Punctuator::LeftBracket)) => Some(PostfixOperator::Subscript),
            span!(Token::Punctuator(Punctuator::Question)) => Some(PostfixOperator::Ternary),

            // span!(Token::Punctuator(Punctuator::LeftParen)) => Some(PostfixOperator::Call),
            _ => None,
        }
    }
    pub fn parse_expression(&mut self) -> PResult<Spanned<Expression>> {
        self.parse_expression_bp(0)
    }
    /// THANKS!!! https://matklad.github.io/2020/04/13/simple-but-powerful-pratt-parsing.html
    pub fn parse_expression_bp(
        &mut self,
        minimum_binding_power: u8,
    ) -> PResult<Spanned<Expression>> {
        if self.peek_token().is_err() {
            return Ok(Spanned::new(Location::new(), Expression::Null));
        }
        let as_prefix_operator = Self::as_prefix_operator(self.peek_token()?);

        let mut lhs = if let Some(prefix_operator) = as_prefix_operator {
            let ((), right_binding_power) = Self::prefix_binding_power(&prefix_operator);

            let span!(prefix_op_span, _) = self.next_token()?; // bump prefix operator
            let can_start_type_name = self.can_start_typename();

            let expression = match (&prefix_operator, can_start_type_name) {
                (PrefixOperator::SizeOf, _) => {
                    let _ = self.expect_token(
                        Token::Punctuator(Punctuator::LeftParen),
                        "to open sizeof expression",
                    )?;
                    let type_name = self.parse_typename()?;
                    let closer = self.expect_token(
                        Token::Punctuator(Punctuator::RightParen),
                        "to close sizeof expression",
                    )?;
                    prefix_op_span
                        .until(&closer.location)
                        .into_spanned(Expression::Sizeof(type_name))
                }
                (PrefixOperator::CastOrParen, true) => {
                    let type_name = self.parse_typename()?;
                    self.expect_token(
                        Token::Punctuator(Punctuator::RightParen),
                        "to close cast expression",
                    )?;
                    let expr = self.parse_expression_bp(right_binding_power)?;
                    type_name
                        .location
                        .until(&expr.location)
                        .into_spanned(Expression::Cast(type_name, Box::new(expr)))
                }
                (PrefixOperator::CastOrParen, false) => {
                    let expr = self.parse_expression()?;
                    self.expect_token(
                        Token::Punctuator(Punctuator::RightParen),
                        "to close paren expression",
                    )?;
                    expr
                }
                _ => {
                    let expression = self.parse_expression_bp(right_binding_power)?;
                    prefix_op_span
                        .until(&expression.location)
                        .into_spanned(Expression::UnaryOp(
                            Spanned::new(prefix_op_span.clone(), prefix_operator),
                            Box::new(expression),
                        ))
                }
            };

            expression
        } else {
            match self.next_token()? {
                span!(span, Token::Identifier(identifier)) => {
                    span.to_spanned(Expression::Identifier(span.to_spanned(identifier)))
                }
                span!(span, Token::Constant(iconst)) => {
                    span.to_spanned(Expression::IntConstant(span.to_spanned(iconst)))
                }
                span!(span, Token::StringLiteral(iconst)) => {
                    span.to_spanned(Expression::StringLiteral(span.to_spanned(iconst)))
                }
                s => {
                    return Err(SimpleError(
                        s.location,
                        "unrecognized char while parsing expression".into(),
                    )
                    .into_codespan_report());
                }
            }
        };

        while !self.next_is_semicolon() {
            // an expression can end on EOF when called from an #if directive, just end the loop early in that case.
            let peeked = match self.peek_token() {
                Ok(tok) => tok,
                Err(_) => break,
            };
            let post_fix_operator = Self::as_postfix_operator(peeked);

            if let Some(post_fix_operator) = post_fix_operator {
                let (left_binding_power, ()) = Self::postfix_binding_power(&post_fix_operator);
                if left_binding_power < minimum_binding_power {
                    break;
                }
                let span!(post_fix_operator_span, _) = self.next_token()?; // bump past postfix operator

                lhs =
                    match post_fix_operator {
                        PostfixOperator::Call => {
                            let args = self.parse_argument_expression_list()?;
                            post_fix_operator_span
                                .until(&lhs.location)
                                .into_spanned(Expression::Call(Box::new(lhs), args))
                        }
                        PostfixOperator::Subscript => {
                            let expr = self.parse_expression()?;
                            let closer = self.expect_token(
                                Token::Punctuator(Punctuator::RightBracket),
                                "to close array subscript",
                            )?;
                            lhs.location
                                .until(&closer.location)
                                .into_spanned(Expression::Subscript(Box::new(lhs), Box::new(expr)))
                        }
                        PostfixOperator::Ternary => {
                            let then = self.parse_expression()?;
                            let seperator = self.expect_token(
                                Token::Punctuator(Punctuator::Colon),
                                "to seperate ternary true and false branches",
                            )?;
                            let elze = self.parse_expression_bp(left_binding_power)?;

                            lhs.location.until(&seperator.location).into_spanned(
                                Expression::Ternary(Box::new(lhs), Box::new(then), Box::new(elze)),
                            )
                        }
                        _ => post_fix_operator_span.until(&lhs.location).into_spanned(
                            Expression::PostfixOp(
                                post_fix_operator_span.into_spanned(post_fix_operator),
                                Box::new(lhs),
                            ),
                        ),
                    };
                continue;
            }

            if let Some(infix_operator) = Self::as_infix_operator(peeked) {
                let (left_binding_power, right_binding_power) =
                    Self::infix_binding_power(&infix_operator).unwrap();

                if left_binding_power < minimum_binding_power {
                    break;
                }
                let span!(infix_operator_span, _) = self.next_token()?; // bump infix operator

                let rhs = self.parse_expression_bp(right_binding_power)?;
                lhs = lhs
                    .location
                    .until(&rhs.location)
                    .into_spanned(Expression::BinOp(
                        infix_operator_span.into_spanned(infix_operator),
                        Box::new(lhs),
                        Box::new(rhs),
                    ));
                continue;
            }
            break;
        }

        Ok(lhs)
    }
    pub fn parse_argument_expression_list(&mut self) -> PResult<Vec<Spanned<Expression>>> {
        let mut arguments = vec![];
        while !self.eat_if_next(Token::Punctuator(Punctuator::RightParen))?
            || self.consider_comma(&arguments)?
        {
            if arguments.len() > 0 {
                self.expect_token(
                    Token::Punctuator(Punctuator::Comma),
                    "to seperate arguments",
                )?;
            }

            // recover from trailing comma
            if let Some(token) = self.eat_next(Token::Punctuator(Punctuator::RightParen))? {
                self.diagnostics.push(
                    SimpleError(token.location, format!("Trailing comma is not allowed"))
                        .into_codespan_report(),
                );
                break;
            }
            arguments.push(self.parse_expression()?);
        }
        Ok(arguments)
    }
    pub fn can_start_typename(&mut self) -> bool {
        match self.peek_token().cloned() {
            Ok(span!(Token::Keyword(type_specifier!() | type_qualifier!()))) => true,
            Ok(span!(Token::Identifier(identifier))) if self.is_typedef(&identifier) => true,
            _ => false,
        }
    }

    pub fn can_start_primary_expression(&mut self) -> bool {
        match self.peek_token().cloned() {
            Ok(span!(Token::Identifier(identifier))) => !self.is_typedef(&identifier),
            Ok(span!(Token::Constant(_))) => true,
            Ok(span!(Token::StringLiteral(_))) => true,
            Ok(span!(Token::Punctuator(Punctuator::LeftParen))) => true,
            Ok(t) if Self::as_prefix_operator(&t).is_some() => true,
            _ => false,
        }
    }
    pub fn parse_typename(&mut self) -> PResult<Spanned<TypeName>> {
        // todo: add spanned to parse_specifier_qualifier_list
        let start = self.last_location.clone();
        let sq = self.parse_specifier_qualifier_list()?;
        assert!(sq.len() > 0);
        let d = self.parse_abstract_declarator()?;

        Ok(start.until(&d.location).into_spanned(TypeName {
            specifier_qualifiers: sq,
            declarator: d,
        }))
    }
    pub fn next_is_semicolon(&mut self) -> bool {
        matches!(
            self.peek_token(),
            Ok(span!(Token::Punctuator(Punctuator::Semicolon)))
        )
    }
    pub fn next_is_eof(&mut self) -> bool {
        matches!(self.peek_token(), Err(_))
    }
}
