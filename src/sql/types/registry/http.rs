use crate::model::DataType;

use super::FunctionRegistry;

/// HTTP extension function names that should be excluded from the pg_proc
/// builtin enumeration (they appear via the extension-specific path instead).
pub(crate) const HTTP_FUNCTION_NAMES: &[&str] = &[
    "HTTP",
    "HTTP_GET",
    "HTTP_HEAD",
    "HTTP_DELETE",
    "HTTP_POST",
    "HTTP_PUT",
    "HTTP_PATCH",
];

pub fn register(r: &mut FunctionRegistry) {
    use super::FunctionSignature;

    let jsonb = DataType::Jsonb;

    // http_get(url text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_GET",
        FunctionSignature::fixed(jsonb.clone()).with_args(1, Some(2)),
    );
    // http_head(url text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_HEAD",
        FunctionSignature::fixed(jsonb.clone()).with_args(1, Some(2)),
    );
    // http_delete(url text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_DELETE",
        FunctionSignature::fixed(jsonb.clone()).with_args(1, Some(2)),
    );
    // http_post(url text, body text, content_type text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_POST",
        FunctionSignature::fixed(jsonb.clone()).with_args(3, Some(4)),
    );
    // http_put(url text, body text, content_type text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_PUT",
        FunctionSignature::fixed(jsonb.clone()).with_args(3, Some(4)),
    );
    // http_patch(url text, body text, content_type text [, headers jsonb]) → jsonb
    r.register(
        "HTTP_PATCH",
        FunctionSignature::fixed(jsonb.clone()).with_args(3, Some(4)),
    );
    // http(method text, uri text [, headers jsonb [, content_type text [, content text]]]) → jsonb
    r.register(
        "HTTP",
        FunctionSignature::fixed(jsonb).with_args(2, Some(5)),
    );
}
