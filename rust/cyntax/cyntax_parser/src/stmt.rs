use cyntax_common::ast::{Keyword, Punctuator};
use cyntax_common::span;
use cyntax_common::spanned::Spanned;
use cyntax_errors::Diagnostic;
use cyntax_errors::errors::SimpleError;

use crate::ast::{
    BlockItem, ForInit, Initializer, IterationStatement, LabeledStatement, Statement, Token,
};
use crate::{PResult, Parser};

impl<'src> Parser<'src> {
    pub fn parse_statement(&mut self) -> PResult<Spanned<Statement>> {
        if self.can_start_labeled_stmt() {
            return Ok(self.parse_labeled_statement()?);
        } else if self.can_start_compound_statement() {
            return Ok(self.parse_compound_statement()?);
        } else if self.can_start_primary_expression() {
            let expr = self.parse_expression()?;
            let semi =
                self.expect_token(Token::Punctuator(Punctuator::Semicolon), "after expression")?;
            return Ok(expr
                .location
                .until(&semi.location)
                .into_spanned(Statement::Expression(expr)));
        } else if self.can_start_selection_statement() {
            return Ok(self.parse_selection_statement()?);
        } else if self.can_start_iteration_statement() {
            return Ok(self.parse_iteration_statement()?);
        } else if self.can_start_jump_statement() {
            return self.parse_jump_statement();
        }
        Err(SimpleError(
            self.last_location.clone(),
            format!(
                "failed to parse statement starting with {:?}",
                self.peek_token()
            ),
        )
        .into_codespan_report())
    }
    pub fn can_start_labeled_stmt(&mut self) -> bool {
        let a = matches!(self.peek_token(), Ok(span!(Token::Identifier(_))))
            && matches!(
                self.peek_token_nth(1),
                Ok(span!(Token::Punctuator(Punctuator::Colon)))
            );
        return a
            || matches!(
                self.peek_token(),
                Ok(span!(Token::Keyword(Keyword::Case | Keyword::Default)))
            );
    }

    /// INVALID
    /// ``
    /// test:
    /// int a = 5;
    /// ``
    ///
    /// VALID
    /// ``
    /// test:
    /// a = 5;``
    ///
    /// VALID
    /// ``
    /// test: {
    /// int a = 5;
    /// }``
    pub fn check_stmt_not_declaration(&mut self) -> PResult<()> {
        if self.can_start_declaration_specifier() && !self.can_start_statement() {
            let loc = self.maybe_recover(
                |this| Ok(this.parse_declaration()?.location),
                |this| this.last_location.clone(),
                Token::Punctuator(Punctuator::Semicolon),
            );
            Err(SimpleError(loc, format!("Labels must be followed by a statement, not declaration. try wrapping the declaration in a block `{{` `}}`")).into_codespan_report())
        } else {
            Ok(())
        }
    }
    pub fn parse_labeled_statement(&mut self) -> PResult<Spanned<Statement>> {
        if let Some(labeled_stmt) = self.eat_next(Token::Keyword(Keyword::Case))? {
            let expr = self.parse_expression()?;
            self.expect_token(
                Token::Punctuator(Punctuator::Colon),
                "to seperate case expreesion and it's statement",
            )?;
            self.check_stmt_not_declaration()?;
            let stmt = self.parse_statement()?;

            Ok(labeled_stmt
                .location
                .until(&stmt.location)
                .into_spanned(Statement::Labeled(crate::ast::LabeledStatement::Case(
                    expr,
                    Box::new(stmt),
                ))))
        } else if let Some(default) = self.eat_next(Token::Keyword(Keyword::Default))? {
            let span!(colon, _) = self.expect_token(
                Token::Punctuator(Punctuator::Colon),
                "to seperate `default` case and it's statement",
            )?;
            self.check_stmt_not_declaration()?;
            let stmt = self.parse_statement()?;
            Ok(default
                .location
                .until(&colon)
                .into_spanned(Statement::Labeled(LabeledStatement::Default(Box::new(
                    stmt,
                )))))
        } else {
            let identifier = self.expect_non_typename_identifier()?;
            self.expect_token(
                Token::Punctuator(Punctuator::Colon),
                &format!("to seperate `{}` case and it's statement", identifier.value),
            )?;
            self.check_stmt_not_declaration()?;
            let stmt = self.parse_statement()?;
            Ok(identifier
                .location
                .until(&stmt.location)
                .into_spanned(Statement::Labeled(LabeledStatement::Identifier(
                    identifier,
                    Box::new(stmt),
                ))))
        }
    }

    pub fn can_start_compound_statement(&mut self) -> bool {
        return matches!(
            self.peek_token(),
            Ok(span!(Token::Punctuator(Punctuator::LeftBrace)))
        );
    }

    pub fn parse_compound_statement(&mut self) -> PResult<Spanned<Statement>> {
        self.push_scope(crate::ScopeKind::Block);
        let opener = self.expect_token(
            Token::Punctuator(Punctuator::LeftBrace),
            "to open compound statement",
        )?;
        let mut block_items = vec![];
        while self.can_start_block_item() {
            let block_item = self.parse_block_item()?;
            block_items.push(block_item);
        }
        let closer = self.expect_token(
            Token::Punctuator(Punctuator::RightBrace),
            "after compound stmt",
        )?;
        self.pop_scope();

        Ok(opener
            .location
            .until(&closer.location)
            .into_spanned(Statement::Compound(block_items)))
    }
    pub fn can_start_block_item(&mut self) -> bool {
        self.can_start_declaration_specifier() || self.can_start_statement()
    }
    pub fn can_start_statement(&mut self) -> bool {
        self.can_start_labeled_stmt()
            || self.can_start_compound_statement()
            || self.can_start_selection_statement()
            || self.can_start_iteration_statement()
            || self.can_start_jump_statement()
            || self.can_start_primary_expression()
    }
    pub fn parse_block_item(&mut self) -> PResult<BlockItem> {
        if self.can_start_declaration_specifier() {
            let decl = self.parse_declaration()?;
            self.expect_token(
                Token::Punctuator(Punctuator::Semicolon),
                "after each declaration",
            )?;
            Ok(BlockItem::Declaration(decl))
        } else if self.can_start_statement() {
            let stmt = self.parse_statement()?;
            Ok(BlockItem::Statement(stmt))
        } else {
            todo!()
        }
    }
    pub fn can_start_selection_statement(&mut self) -> bool {
        return matches!(
            self.peek_token(),
            Ok(span!(Token::Keyword(Keyword::If | Keyword::Switch)))
        );
    }
    pub fn parse_selection_statement(&mut self) -> PResult<Spanned<Statement>> {
        if let Some(span!(if_loc, _)) = self.eat_next(Token::Keyword(Keyword::If))? {
            self.expect_token(
                Token::Punctuator(Punctuator::LeftParen),
                "to open if statement's condition expression",
            )?;
            let expr = self.parse_expression()?;
            self.expect_token(
                Token::Punctuator(Punctuator::RightParen),
                "to close if statement's condition expression",
            )?;
            let then = self.parse_statement()?;
            if self.eat_if_next(Token::Keyword(Keyword::Else))? {
                let elze = self.parse_statement()?;
                return Ok(if_loc.until(&elze.location).into_spanned(Statement::If(
                    expr,
                    Box::new(then),
                    Some(Box::new(elze)),
                )));
            } else {
                return Ok(if_loc.until(&then.location).into_spanned(Statement::If(
                    expr,
                    Box::new(then),
                    None,
                )));
            }
        } else if let Some(span!(switch_loc, _)) = self.eat_next(Token::Keyword(Keyword::Switch))? {
            self.expect_token(
                Token::Punctuator(Punctuator::LeftParen),
                "to open switch statement's condition expression",
            )?;
            let expr = self.parse_expression()?;
            self.expect_token(
                Token::Punctuator(Punctuator::RightParen),
                "to close switch statement's condition expression",
            )?;
            let stmt = self.parse_statement()?;

            return Ok(switch_loc
                .until(&stmt.location)
                .into_spanned(Statement::Switch(expr, Box::new(stmt))));
        } else {
            todo!()
        }
    }
    pub fn can_start_iteration_statement(&mut self) -> bool {
        return matches!(
            self.peek_token(),
            Ok(span!(Token::Keyword(
                Keyword::While | Keyword::Do | Keyword::For
            )))
        );
    }
    pub fn parse_iteration_statement(&mut self) -> PResult<Spanned<Statement>> {
        if let Some(span!(while_loc, _)) = self.eat_next(Token::Keyword(Keyword::While))? {
            self.expect_token(
                Token::Punctuator(Punctuator::LeftParen),
                "to open condition part of while statement",
            )?;
            let condition = self.parse_expression()?;
            self.expect_token(
                Token::Punctuator(Punctuator::RightParen),
                "to close condition part of while statement",
            )?;
            let body = self.parse_statement()?;
            return Ok(while_loc
                .until(&body.location)
                .into_spanned(Statement::Iteration(IterationStatement::While(
                    condition,
                    Box::new(body),
                ))));
        } else if let Some(span!(do_loc, _)) = self.eat_next(Token::Keyword(Keyword::Do))? {
            let body = self.parse_statement()?;
            self.expect_token(
                Token::Keyword(Keyword::While),
                "expected while after stmt for do {} while loop",
            )?;

            self.expect_token(
                Token::Punctuator(Punctuator::LeftParen),
                "to open condition part of do while statement",
            )?;
            let condition = self.parse_expression()?;
            self.expect_token(
                Token::Punctuator(Punctuator::RightParen),
                "to close condition part of do while statement",
            )?;
            let span!(semi_loc, _) = self.expect_token(
                Token::Punctuator(Punctuator::Semicolon),
                "to close do while loop",
            )?;

            return Ok(do_loc.until(&semi_loc).into_spanned(Statement::Iteration(
                IterationStatement::DoWhile(Box::new(body), condition),
            )));
        } else if let Some(span!(for_loc, _)) = self.eat_next(Token::Keyword(Keyword::For))? {
            self.expect_token(
                Token::Punctuator(Punctuator::LeftParen),
                "to open for statement",
            )?;

            let init = if self.eat_if_next(Token::Punctuator(Punctuator::Semicolon))? {
                None
            } else {
                if self.can_start_declaration_specifier() {
                    let decl = self.parse_declaration()?;
                    self.expect_token(
                        Token::Punctuator(Punctuator::Semicolon),
                        "after for loop init",
                    )?;
                    Some(ForInit::Declaration(decl))
                } else {
                    let expr = self.parse_expression()?;
                    self.expect_token(
                        Token::Punctuator(Punctuator::Semicolon),
                        "after for loop init",
                    )?;
                    Some(ForInit::Expression(expr))
                }
            };

            let condition = if self.eat_if_next(Token::Punctuator(Punctuator::Semicolon))? {
                None
            } else {
                let expr = self.parse_expression()?;
                self.expect_token(
                    Token::Punctuator(Punctuator::Semicolon),
                    "after condition expression in for loop",
                )?;
                Some(expr)
            };

            let update = if self.eat_if_next(Token::Punctuator(Punctuator::RightParen))? {
                None
            } else {
                let expr = self.parse_expression()?;
                self.expect_token(
                    Token::Punctuator(Punctuator::RightParen),
                    "to close for statement",
                )?;
                Some(expr)
            };

            let body = self.parse_statement()?;
            return Ok(for_loc
                .until(&body.location)
                .into_spanned(Statement::Iteration(IterationStatement::ForLoop {
                    init,
                    condition,
                    update,
                    body: Box::new(body),
                })));
        }
        todo!()
    }
    pub fn can_start_jump_statement(&mut self) -> bool {
        return matches!(
            self.peek_token(),
            Ok(span!(Token::Keyword(
                Keyword::Goto | Keyword::Continue | Keyword::Break | Keyword::Return
            )))
        );
    }
    pub fn parse_jump_statement(&mut self) -> PResult<Spanned<Statement>> {
        let jump_stmt = match self.next_token()? {
            span!(loc, Token::Keyword(Keyword::Goto)) => {
                let identifier = self.expect_non_typename_identifier()?;
                loc.until(&identifier.location)
                    .into_spanned(Statement::Goto(identifier))
            }
            span!(loc, Token::Keyword(Keyword::Continue)) => loc.into_spanned(Statement::Continue),
            span!(loc, Token::Keyword(Keyword::Break)) => loc.into_spanned(Statement::Break),
            span!(loc, Token::Keyword(Keyword::Return)) => {
                if matches!(
                    self.peek_token(),
                    Ok(span!(Token::Punctuator(Punctuator::Semicolon)))
                ) {
                    loc.into_spanned(Statement::Return(None))
                } else if self.can_start_primary_expression() {
                    let e = self.parse_expression()?;
                    loc.until(&e.location)
                        .into_spanned(Statement::Return(Some(e)))
                } else {
                    return Err(SimpleError(
                        self.last_location.clone(),
                        "cannot start primary expression".to_string(),
                    )
                    .into_codespan_report());
                }
            }
            _ => unreachable!(),
        };
        self.expect_token(
            Token::Punctuator(Punctuator::Semicolon),
            "after jump statement   ",
        )?;

        Ok(jump_stmt)
    }
}
