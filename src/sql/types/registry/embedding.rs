use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    r.register(
        "EMBEDDING",
        FunctionSignature::custom(|_args| DataType::Vector(0)).with_args(1, Some(3)),
    );
}
