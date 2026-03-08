use super::FunctionSignature;
use crate::model::DataType;

pub(super) fn register(r: &mut super::FunctionRegistry) {
    // EMBEDDING(text TEXT [, model TEXT, dimensions BIGINT]) -> VECTOR
    r.register(
        "EMBEDDING",
        FunctionSignature::custom(|_args| DataType::Vector(0)).with_args(1, Some(3)),
    );

    // EMBED_TEXT(model TEXT, text TEXT [, json_options TEXT]) -> VECTOR
    // TiDB-compatible auto-embedding function.
    r.register(
        "EMBED_TEXT",
        FunctionSignature::custom(|_args| DataType::Vector(0)).with_args(2, Some(3)),
    );
}
