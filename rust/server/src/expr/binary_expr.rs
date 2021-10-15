use crate::array::{BoolArray, F32Array, F64Array, I16Array, I32Array, I64Array, UTF8Array};
use crate::expr::expr_tmpl::BinaryExpression;
use crate::expr::BoxedExpression;
use crate::types::{DataTypeKind, DataTypeRef};
use crate::vector_op::cmp::{
    primitive_eq, primitive_geq, primitive_gt, primitive_leq, primitive_lt, primitive_neq,
};
use crate::vector_op::like::like_default;
use crate::vector_op::position::position;
use risingwave_proto::expr::ExprNode_Type;
use std::marker::PhantomData;

/// The macro is responsible for specializing expressions according to the left expr and right expr.
/// Parameters:
///   `l`/`r`: the left/right child of the binary expression
///   `ret`: the return type of the binary expression
///   `int_f`/`float_f`: the scalar func for the binary
/// returns:
///   Boxed Expression
///
/// Note for scalar func:
/// For some scalar functions, the operations are different with different types, we can not put them in one generic function
///   e.g. adding for int is different from that for float
/// Thus, we should manually specialize the scalar function according to different type and pass them to the macro.
///
// TODO: Simplify using macro
macro_rules! gen_across_binary {
    ($l:expr, $r:expr, $ret: expr, $OA: ty, $int_f:ident, $float_f: ident) => {
        match (
            $l.return_type().data_type_kind(),
            $r.return_type().data_type_kind(),
        ) {
            // integer
            (DataTypeKind::Int16, DataTypeKind::Int16) => {
                Box::new(BinaryExpression::<I16Array, I16Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i16, i16, i16>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int16, DataTypeKind::Int32) => {
                Box::new(BinaryExpression::<I16Array, I32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i16, i32, i32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int16, DataTypeKind::Int64) => {
                Box::new(BinaryExpression::<I16Array, I64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i16, i64, i64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int16, DataTypeKind::Float32) => {
                Box::new(BinaryExpression::<I16Array, F32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i16, f32, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int16, DataTypeKind::Float64) => {
                Box::new(BinaryExpression::<I16Array, F64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i16, f64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int32, DataTypeKind::Int32) => {
                Box::new(BinaryExpression::<I32Array, I32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i32, i32, i32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int32, DataTypeKind::Int64) => {
                Box::new(BinaryExpression::<I32Array, I64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i32, i64, i64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int32, DataTypeKind::Float32) => {
                Box::new(BinaryExpression::<I32Array, F32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i32, f32, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int32, DataTypeKind::Float64) => {
                Box::new(BinaryExpression::<I32Array, F64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i32, f64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int64, DataTypeKind::Int64) => {
                Box::new(BinaryExpression::<I64Array, I64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i64, i64, i64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int64, DataTypeKind::Float32) => {
                Box::new(BinaryExpression::<I64Array, F32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i64, f32, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int64, DataTypeKind::Float64) => {
                Box::new(BinaryExpression::<I64Array, F64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<i64, f64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float32, DataTypeKind::Float32) => {
                Box::new(BinaryExpression::<F32Array, F32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f32, f32, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float32, DataTypeKind::Float64) => {
                Box::new(BinaryExpression::<F32Array, F64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f32, f64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float64, DataTypeKind::Float64) => {
                Box::new(BinaryExpression::<F64Array, F64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f64, f64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int32, DataTypeKind::Int16) => {
                Box::new(BinaryExpression::<I32Array, I16Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i32, i16, i32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int64, DataTypeKind::Int16) => {
                Box::new(BinaryExpression::<I64Array, I16Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i64, i16, i64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Int64, DataTypeKind::Int32) => {
                Box::new(BinaryExpression::<I64Array, I32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $int_f::<i64, i32, i64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float32, DataTypeKind::Int16) => {
                Box::new(BinaryExpression::<F32Array, I16Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f32, i16, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float32, DataTypeKind::Int32) => {
                Box::new(BinaryExpression::<F32Array, I32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f32, i32, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float32, DataTypeKind::Int64) => {
                Box::new(BinaryExpression::<F32Array, I64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f32, i64, f32>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float64, DataTypeKind::Int16) => {
                Box::new(BinaryExpression::<F64Array, I16Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f64, i16, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float64, DataTypeKind::Int32) => {
                Box::new(BinaryExpression::<F64Array, I32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f64, i32, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float64, DataTypeKind::Int64) => {
                Box::new(BinaryExpression::<F64Array, I64Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f64, i64, f64>,
                    _phantom: PhantomData,
                })
            }
            (DataTypeKind::Float64, DataTypeKind::Float32) => {
                Box::new(BinaryExpression::<F64Array, F32Array, $OA, _> {
                    expr_ia1: $l,
                    expr_ia2: $r,
                    return_type: $ret,
                    func: $float_f::<f64, f32, f64>,
                    _phantom: PhantomData,
                })
            }
            _ => {
                unimplemented!(
                    "The expression using vectorized expression framework is not supported yet!"
                )
            }
        }
    };
}

pub fn new_binary_expr(
    expr_type: ExprNode_Type,
    ret: DataTypeRef,
    l: BoxedExpression,
    r: BoxedExpression,
) -> BoxedExpression {
    match expr_type {
        ExprNode_Type::EQUAL => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_eq, primitive_eq)
        }
        ExprNode_Type::NOT_EQUAL => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_neq, primitive_neq)
        }
        ExprNode_Type::LESS_THAN => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_lt, primitive_lt)
        }
        ExprNode_Type::GREATER_THAN => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_gt, primitive_gt)
        }
        ExprNode_Type::GREATER_THAN_OR_EQUAL => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_geq, primitive_geq)
        }
        ExprNode_Type::LESS_THAN_OR_EQUAL => {
            gen_across_binary!(l, r, ret, BoolArray, primitive_leq, primitive_leq)
        }
        _ => {
            unimplemented!(
                "The expression using vectorized expression framework is not supported yet!"
            )
        }
    }
}

pub fn new_like_default(
    expr_ia1: BoxedExpression,
    expr_ia2: BoxedExpression,
    return_type: DataTypeRef,
) -> BoxedExpression {
    Box::new(BinaryExpression::<UTF8Array, UTF8Array, BoolArray, _> {
        expr_ia1,
        expr_ia2,
        return_type,
        func: like_default,
        _phantom: PhantomData,
    })
}

pub fn new_position_expr(
    expr_ia1: BoxedExpression,
    expr_ia2: BoxedExpression,
    return_type: DataTypeRef,
) -> BoxedExpression {
    Box::new(BinaryExpression::<UTF8Array, UTF8Array, I32Array, _> {
        expr_ia1,
        expr_ia2,
        return_type,
        func: position,
        _phantom: PhantomData,
    })
}
