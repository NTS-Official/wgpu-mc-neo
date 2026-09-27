use cyntax_common::{ctx::ParseContext, span, spanned::Spanned};
use cyntax_errors::{Diagnostic, errors::SimpleError};
use cyntax_parser::{
    ast::{Expression, InfixOperator, PrefixOperator},
    constant::IntConstant,
};

type PResult<T> = Result<T, cyntax_errors::codespan_reporting::diagnostic::Diagnostic<usize>>;
#[derive(Debug)]
pub struct ConstantEvalutator<'src> {
    ctx: &'src mut ParseContext,
}
#[derive(Debug, Clone, Copy)]
pub enum Value {
    Int(i64),
}
impl<'src> ConstantEvalutator<'src> {
    pub fn new(ctx: &'src mut ParseContext) -> Self {
        Self { ctx }
    }
    pub fn evaluate(&mut self, expr: &Spanned<Expression>) -> PResult<Value> {
        #[allow(unused_variables)]
        match expr {
            span!(Expression::Identifier(identifier)) if identifier.value == "true" => {
                Ok(Value::Int(1))
            }
            span!(Expression::Identifier(identifier)) if identifier.value == "false" => {
                Ok(Value::Int(0))
            }
            // undefined macros expand to 0, we assume every identifier at this point is an undefined macro
            span!(Expression::Identifier(_)) => Ok(Value::Int(0)),
            span!(Expression::IntConstant(constant)) => self.constant(constant),
            span!(Expression::StringLiteral(string)) => Self::not_const(string),
            span!(Expression::Parenthesized(expr)) => self.evaluate(expr),
            span!(Expression::BinOp(op, lhs, rhs)) => self.bin_op(op, lhs, rhs),
            span!(Expression::UnaryOp(op, expr)) => self.un_op(op, expr),
            span!(Expression::PostfixOp(op, xpr)) => todo!(),
            span!(Expression::Cast(ty, expr)) => todo!(),
            span!(Expression::Call(target, paremeters)) => todo!(),
            span!(Expression::Subscript(spanned, spanned1)) => todo!(),
            span!(Expression::Ternary(cond, then, elze)) => self.ternary(cond, then, elze),
            span!(Expression::Sizeof(type_name)) => todo!(),
            span!(Expression::Null) => Ok(Value::Int(0)),
        }
    }
    fn not_const<T>(x: &Spanned<T>) -> PResult<Value> {
        Err(SimpleError(x.location.clone(), "not constant".to_string()).into_codespan_report())
    }
    fn constant(&mut self, constant: &Spanned<IntConstant>) -> PResult<Value> {
        let val = i64::from_str_radix(&constant.value.number, constant.value.base.into()).unwrap();
        Ok(Value::Int(val))
    }
    fn bin_op(
        &mut self,
        op: &Spanned<InfixOperator>,
        left: &Spanned<Expression>,
        right: &Spanned<Expression>,
    ) -> PResult<Value> {
        match op {
            span!(InfixOperator::LogicalAnd) => {
                let left_val = self.evaluate(left)?;
                if !left_val.bool()? {
                    return Ok(Value::from_bool(false));
                }
                let right_val = self.evaluate(right)?;
                Ok(Value::from_bool(right_val.bool()?))
            }
            span!(InfixOperator::LogicalOr) => {
                let left_val = self.evaluate(left)?;
                if left_val.bool()? {
                    return Ok(Value::from_bool(true));
                }
                let right_val = self.evaluate(right)?;
                Ok(Value::from_bool(right_val.bool()?))
            }
            _ => {
                let left_val = self.evaluate(left)?;
                let right_val = self.evaluate(right)?;
                match op {
                    span!(InfixOperator::Add) => left_val.add(right_val),
                    span!(InfixOperator::Subtract) => left_val.subtract(right_val),
                    span!(InfixOperator::Multiply) => left_val.multiply(right_val),
                    span!(InfixOperator::Divide) => left_val.divide(right_val),
                    span!(InfixOperator::Modulo) => todo!(),
                    span!(InfixOperator::Less) => left_val.less(right_val),
                    span!(InfixOperator::Greater) => left_val.greater(right_val),
                    span!(InfixOperator::LessEqual) => left_val.less_eq(right_val),
                    span!(InfixOperator::GreaterEqual) => left_val.greater_eq(right_val),
                    span!(InfixOperator::Equal) => Ok(left_val.equal(right_val)?),
                    span!(InfixOperator::NotEqual) => Ok(left_val.equal(right_val)?.not()?),
                    span!(InfixOperator::BitwiseAnd) => todo!(),
                    span!(InfixOperator::BitwiseOr) => todo!(),
                    span!(InfixOperator::BitwiseXor) => todo!(),
                    span!(InfixOperator::BitwiseShiftLeft) => left_val.shl(right_val),
                    span!(InfixOperator::BitwiseShiftRight) => left_val.shr(right_val),
                    span!(InfixOperator::Assign) => todo!(),
                    span!(InfixOperator::AddAssign) => todo!(),
                    span!(InfixOperator::SubtractAssign) => todo!(),
                    span!(InfixOperator::MultiplyAssign) => todo!(),
                    span!(InfixOperator::DivideAssign) => todo!(),
                    span!(InfixOperator::ModuloAssign) => todo!(),
                    span!(InfixOperator::BitwiseAndAssign) => todo!(),
                    span!(InfixOperator::BitwiseOrAssign) => todo!(),
                    span!(InfixOperator::BitwiseXorAssign) => todo!(),
                    span!(InfixOperator::BitwiseShiftRightAssign) => todo!(),
                    span!(InfixOperator::BitwiseShiftLeftAssign) => todo!(),
                    span!(InfixOperator::Access) => todo!(),
                    span!(InfixOperator::IndirectAccess) => todo!(),
                    _ => unreachable!(),
                }
            }
        }
    }
    fn un_op(
        &mut self,
        op: &Spanned<PrefixOperator>,
        expr: &Spanned<Expression>,
    ) -> PResult<Value> {
        let expr_val = self.evaluate(expr)?;
        match op {
            span!(PrefixOperator::Plus) => Ok(expr_val), // Unary plus is a no-op for integers
            span!(PrefixOperator::Minus) => expr_val.negate(),
            span!(PrefixOperator::LogicalNot) => expr_val.not(),
            _ => todo!(),
        }
    }
    fn ternary(
        &mut self,
        cond: &Spanned<Expression>,
        then: &Spanned<Expression>,
        elze: &Spanned<Expression>,
    ) -> PResult<Value> {
        let cond = self.evaluate(cond)?;
        if cond.bool()? {
            return self.evaluate(then);
        } else {
            return self.evaluate(elze);
        }
    }
}
impl Value {
    pub fn one() -> Self {
        Self::Int(1)
    }
    pub fn zero() -> Self {
        Self::Int(0)
    }
    pub fn from_bool(b: bool) -> Self {
        if b { Self::one() } else { Self::zero() }
    }
    pub fn add(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(Value::Int(lv + rv)),
        }
    }
    pub fn subtract(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(Value::Int(lv - rv)),
        }
    }
    pub fn multiply(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(Value::Int(lv * rv)),
        }
    }
    pub fn divide(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => {
                if rv == 0 {
                    // Consider returning an error Diagnostic here for division by zero
                    panic!("divide by zero")
                } else {
                    Ok(Value::Int(lv / rv))
                }
            }
        }
    }
    pub fn equal(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(if lv == rv {
                Value::Int(1)
            } else {
                Value::Int(0)
            }),
        }
    }
    pub fn greater(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(if lv > rv {
                Value::Int(1)
            } else {
                Value::Int(0)
            }),
        }
    }
    pub fn greater_eq(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(if lv >= rv {
                Value::Int(1)
            } else {
                Value::Int(0)
            }),
        }
    }
    pub fn less_eq(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(if lv <= rv {
                Value::Int(1)
            } else {
                Value::Int(0)
            }),
        }
    }
    pub fn less(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(rv)) => Ok(if lv < rv {
                Value::Int(1)
            } else {
                Value::Int(0)
            }),
        }
    }
    pub fn negate(self) -> PResult<Self> {
        match self {
            Value::Int(v) => Ok(Value::Int(-v)),
        }
    }
    pub fn not(self) -> PResult<Self> {
        match self {
            Value::Int(1) => Ok(Value::Int(0)),
            Value::Int(0) => Ok(Value::Int(1)),
            Value::Int(_) => panic!("todo: real error here. just curious if its ever triggered"),
        }
    }
    pub fn bool(self) -> PResult<bool> {
        match self {
            Value::Int(value) => Ok(value != 0),
        }
    }
    pub fn shl(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(right)) => Ok(Value::Int(lv << right)),
        }
    }
    pub fn shr(self, other: Self) -> PResult<Self> {
        match (self, other) {
            (Value::Int(lv), Value::Int(right)) => Ok(Value::Int(lv >> right)),
        }
    }
}
