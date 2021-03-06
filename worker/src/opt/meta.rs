use crate::common::{Deserialize, Serialize};

use entish::prelude::*;

pub use super::*;
use core::fmt;

pub trait RelRepr<E>: Sized + Send + Sync {
    fn dot(node: GenericRel<&E, &Self>) -> ValidateResult<Self>;
}

pub trait ExprRepr: Sized + Send + Sync + Clone {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self>;
}

macro_rules! error {
    ($variant:ident $(, $t:tt)+) => {
        Err(ValidateError::$variant($($t.to_string(),)+))
    };
    ($variant:ident) => {
        ValidateError:$variant
    }
}

impl<E, T> RelRepr<E> for Option<T>
where
    T: RelRepr<E>,
{
    fn dot(node: GenericRel<&E, &Self>) -> ValidateResult<Self> {
        node.map(&mut |child| child.as_ref())
            .into_option()
            .map(|res| T::dot(res))
            .transpose()
    }
}

pub trait Named {
    fn name(&self) -> Option<&str> {
        None
    }
}

impl<E: Named + Send + Sync + PartialEq + Clone> RelRepr<E> for Context<E> {
    fn dot(node: GenericRel<&E, &Self>) -> ValidateResult<Self> {
        match node {
            GenericRel::Table(Table(_)) => Err(ValidateError::Internal(
                "tried to complete from leaf".to_string(),
            )),
            GenericRel::WithAlias(WithAlias { from, alias }) => {
                let ctx = from
                    .iter()
                    .map(|(k, v)| (k.with_prefix(&alias), v.clone()))
                    .collect();
                Ok(ctx)
            }
            GenericRel::Projection(Projection { attributes, from })
            | GenericRel::Aggregation(Aggregation {
                attributes, from, ..
            }) => {
                let ctx: Context<_> = attributes
                    .iter()
                    .enumerate()
                    .map(|(i, expr)| {
                        let key = if let Some(alias) = expr.name() {
                            // FIXME sanitization
                            alias.to_string()
                        } else {
                            format!("f{}_", i)
                        };
                        Ok((ContextKey::with_name(&key), (*expr).clone()))
                    })
                    .collect::<ValidateResult<_>>()?;
                Ok(ctx)
            }
            GenericRel::Offset(Offset { from, .. })
            | GenericRel::Limit(Limit { from, .. })
            | GenericRel::OrderBy(OrderBy { from, .. })
            | GenericRel::Distinct(Distinct { from, .. })
            | GenericRel::Selection(Selection { from, .. }) => Ok(from.clone()),
            GenericRel::Join(Join { left, right, .. }) => {
                let mut ctx = left.clone();
                ctx.extend(right.clone());
                Ok(ctx)
            }
            GenericRel::Set(Set { left, right, .. }) => {
                let right = right.clone();
                let ctx: Context<_> = left
                    .clone()
                    .into_iter()
                    .map(|(key, meta)| {
                        let right_m = right.get(&key).map_err(|e| e.into_column_error())?;

                        if meta == *right_m {
                            Ok((ContextKey::with_name(key.name()), meta))
                        } else {
                            Err(ValidateError::SchemaMismatch(key.to_string()))
                        }
                    })
                    .collect::<ValidateResult<_>>()?;
                Ok(ctx)
            }
        }
    }
}

impl<T> ExprRepr for Option<T>
where
    T: ExprRepr,
{
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        node.map(&mut |child| child.as_ref())
            .into_option()
            .map(|res| T::dot(res))
            .transpose()
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Copy, Clone)]
pub enum DataType {
    Integer,
    Float,
    String,
    Boolean,
    Timestamp,
    Date,
    Bytes,
    Null,
}

impl Default for DataType {
    fn default() -> Self {
        Self::Null
    }
}

impl FromStr for DataType {
    type Err = ValidateError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "INTEGER" => Ok(DataType::Integer),
            "FLOAT" => Ok(DataType::Float),
            "STRING" => Ok(DataType::String),
            "BOOLEAN" => Ok(DataType::Boolean),
            "TIMESTAMP" => Ok(DataType::Timestamp),
            "DATE" => Ok(DataType::Date),
            "BYTES" => Ok(DataType::Bytes),
            "NULL" => Ok(DataType::Null),
            _ => Err(ValidateError::UnknownType(value.to_string())),
        }
    }
}

impl DataType {
    pub fn is_numeric(&self) -> bool {
        match self {
            Self::Integer | Self::Float => true,
            _ => false,
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl ExprRepr for DataType {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        match node {
            Expr::Column(Column(ck)) => {
                error!(Internal, (format!("tried to complete a column {}", ck)))
            }
            Expr::Literal(Literal(lit)) => match lit {
                LiteralValue::Long(..) => Ok(DataType::Integer),
                LiteralValue::Double(..) => Ok(DataType::Float),
                LiteralValue::Boolean(..) => Ok(DataType::Boolean),
                LiteralValue::StringLiteral(..) => Ok(DataType::String),
                LiteralValue::Null => Ok(DataType::Null),
            },
            Expr::As(As { expr, .. }) => Ok(expr.clone()),
            Expr::Function(Function { name, mut args, .. }) => {
                let fst = args.pop().ok_or(ValidateError::Expected(
                    "function to have at least one argument".to_string(),
                ))?;
                if args.into_iter().all(|arg| arg == fst) {
                    match name {
                        FunctionName::Count => Ok(DataType::Integer),
                        FunctionName::Sum | FunctionName::Max | FunctionName::Min => {
                            if fst.is_numeric() {
                                Ok(*fst)
                            } else {
                                error!(InvalidType, "numeric type", fst)
                            }
                        }
                        FunctionName::StdDev | FunctionName::Avg => {
                            if fst.is_numeric() {
                                Ok(DataType::Float)
                            } else {
                                error!(InvalidType, "numeric type", fst)
                            }
                        }
                        FunctionName::Concat => match fst {
                            DataType::String => Ok(DataType::String),
                            _ => error!(InvalidType, "string type", fst),
                        },
                    }
                } else {
                    error!(Expected, "all arguments of functions to have the same type")
                }
            }
            Expr::IsNull(..) | Expr::IsNotNull(..) => Ok(DataType::Boolean),
            Expr::InList(InList { expr, list, .. }) => {
                if list.into_iter().any(|elt| elt != expr) {
                    error!(
                        Expected,
                        "in an expression of the form `a IN (b, [c, ..])`, the type of `a`\
                         needs to be the same as the type of each list element `(b, [c, ..])`"
                    )
                } else {
                    Ok(*expr)
                }
            }
            Expr::Between(Between {
                expr, low, high, ..
            }) => {
                if expr.is_numeric() && low.is_numeric() && high.is_numeric() {
                    Ok(DataType::Boolean)
                } else {
                    error!(
                        Expected,
                        "in an expression of the form `a BETWEEN b AND c`, the type of `a`\
                         needs to be the same as the type of both `b` and `c`."
                    )
                }
            }
            Expr::UnaryOp(UnaryOp { op, expr }) => match op {
                UnaryOperator::Plus | UnaryOperator::Minus => {
                    if expr.is_numeric() {
                        Ok(expr.clone())
                    } else {
                        error!(Expected, "the argument of `+` or `-` to be a numeric type")
                    }
                }
                UnaryOperator::Not => {
                    if *expr == DataType::Boolean {
                        Ok(expr.clone())
                    } else {
                        error!(Expected, "the argument of `NOT` to be a boolean")
                    }
                }
            },
            Expr::BinaryOp(BinaryOp { left, op, right }) => match op {
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulus => {
                    if left.is_numeric() && right.is_numeric() {
                        Ok(left.clone())
                    } else {
                        error!(Expected, (format!("the type of both arguments of a binary arithmetic operator \
                                                       expression with operator `{:?}` to both be numeric", op)))
                    }
                }
                BinaryOperator::Gt
                | BinaryOperator::Lt
                | BinaryOperator::GtEq
                | BinaryOperator::LtEq
                | BinaryOperator::Eq
                | BinaryOperator::NotEq => {
                    if left == right {
                        Ok(DataType::Boolean)
                    } else {
                        error!(
                            Expected,
                            "the types of left and right expressions \
                                              in a binary comparison operator to be the same"
                        )
                    }
                }
                BinaryOperator::Like | BinaryOperator::NotLike => {
                    if *left == DataType::String && *right == DataType::String {
                        Ok(DataType::Boolean)
                    } else {
                        error!(
                            Expected,
                            "in an expression of the form `a LIKE b`, both \
                                              `a` and `b` need to be strings"
                        )
                    }
                }
                BinaryOperator::And | BinaryOperator::Or => {
                    if *left == DataType::Boolean && *right == DataType::Boolean {
                        Ok(DataType::Boolean)
                    } else {
                        error!(
                            Expected,
                            "in an expression of the form `a AND b` or `a OR \
                                              b`, both `a` and `b` need to be booleans"
                        )
                    }
                }
            },
            Expr::Case(Case {
                conditions,
                mut results,
                else_results,
                ..
            }) => {
                let fst = results.pop().ok_or(ValidateError::Expected(
                    "at least one `THEN ...` in an expression of the form `CASE`".to_string(),
                ))?;
                if results.into_iter().all(|r| r == fst)
                    && conditions.into_iter().all(|c| *c == DataType::Boolean)
                    && else_results.as_ref().map(|er| *er == fst).unwrap_or(true)
                {
                    Ok(*fst)
                } else {
                    error!(
                        Expected,
                        "in an expression of the form `CASE WHEN a THEN \
                                       b ELSE c`, `a` needs to be a boolean and `b` and \
                                       `c` need to have to same type"
                    )
                }
            }
            Expr::Hash(Hash { .. }) => Ok(DataType::Bytes),
            Expr::Replace(Replace { with, .. }) => Ok(*with),
            Expr::Noisy(Noisy { expr, .. }) => Ok(*expr),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq, Clone)]
pub struct AudienceBoard<V = u64>(HashMap<BlockType, V>);

impl<V> std::default::Default for AudienceBoard<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<V> AudienceBoard<V>
where
    V: std::cmp::PartialOrd,
{
    pub fn into_inner(self) -> HashMap<BlockType, V> {
        self.0
    }

    fn insert(&mut self, at: BlockType, val: V) -> &mut Self {
        if let Some(existing) = self.0.remove(&at) {
            if val > existing {
                self.0.insert(at, val);
            } else {
                self.0.insert(at, existing);
            }
        } else {
            self.0.insert(at, val);
        }
        self
    }

    fn intersect(&mut self, mut other: Self) -> &mut Self {
        let keys: Vec<_> = self.0.keys().cloned().collect();
        for key in keys.into_iter() {
            if let Some(other_val) = other.0.remove(&key) {
                self.insert(key, other_val);
            } else {
                self.0.remove(&key);
            }
        }
        self
    }
}

impl<V> std::iter::FromIterator<(BlockType, V)> for AudienceBoard<V>
where
    V: std::cmp::PartialOrd,
{
    fn from_iter<T: IntoIterator<Item = (BlockType, V)>>(iter: T) -> Self {
        let mut out = Self::default();
        out.extend(iter);
        out
    }
}

impl<V> std::iter::IntoIterator for AudienceBoard<V> {
    type Item = (BlockType, V);
    type IntoIter = std::collections::hash_map::IntoIter<BlockType, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<V> std::iter::Extend<(BlockType, V)> for AudienceBoard<V>
where
    V: std::cmp::PartialOrd,
{
    fn extend<T: IntoIterator<Item = (BlockType, V)>>(&mut self, iter: T) {
        iter.into_iter().for_each(|(bt, val)| {
            self.insert(bt, val);
        })
    }
}

impl ExprRepr for AudienceBoard {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        let mut out = AudienceBoard::default();
        node.map_owned(&mut |child| {
            out.intersect(child.clone());
        });
        Ok(out)
    }
}

impl ExprRepr for HashSet<BlockType> {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        let mut audiences = Vec::new();
        node.map_owned(&mut |child| {
            audiences.push(child);
        });
        let mut audience = audiences
            .pop()
            .map(|aud| aud.clone())
            .unwrap_or(HashSet::new());
        for aud in audiences.into_iter() {
            audience = audience.intersection(aud).cloned().collect();
        }
        Ok(audience)
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, Copy, Clone)]
pub enum Mode {
    Nullable,
    Required,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Mode::Nullable => write!(f, "Nullable"),
            Mode::Required => write!(f, "Required"),
        }
    }
}

impl Default for Mode {
    fn default() -> Self {
        Self::Nullable
    }
}

impl ExprRepr for Mode {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        match node {
            Expr::Column(Column(ck)) => {
                error!(Internal, (format!("tried to complete a column {}", ck)))
            }
            Expr::Literal(Literal(LiteralValue::Null)) => Ok(Self::Nullable),
            Expr::Literal(_) => Ok(Self::Required),
            not_a_leaf => {
                let mut this_nullable = false;
                not_a_leaf.map_owned(&mut |mode| {
                    this_nullable = this_nullable | (*mode == Self::Nullable)
                });
                let out = if this_nullable {
                    Self::Nullable
                } else {
                    Self::Required
                };
                Ok(out)
            }
        }
    }
}

#[derive(Default, Debug, PartialEq, Serialize, Deserialize, Copy, Clone)]
pub struct Taint(pub bool);

impl From<bool> for Taint {
    fn from(val: bool) -> Self {
        Self(val)
    }
}

impl ExprRepr for Taint {
    fn dot(node: Expr<&Self>) -> ValidateResult<Self> {
        let mut taint = false;
        node.map(&mut |child| {
            taint = taint || child.0;
        });
        Ok(Self(taint))
    }
}
