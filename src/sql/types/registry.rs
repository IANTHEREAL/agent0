//! Function signature registry

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::types::DataType;

#[derive(Clone)]
pub enum ReturnType {
    Fixed(DataType),
    SameAsArg(usize),
    FirstNonNull,
    #[allow(dead_code)] // new type inference module, not yet fully integrated
    NumericPromotion,
    Custom(fn(&[DataType]) -> DataType),
}

#[derive(Clone)]
pub struct FunctionSignature {
    pub min_args: usize,
    pub max_args: Option<usize>,
    pub return_type: ReturnType,
    pub is_aggregate: bool,
    pub is_window: bool,
}

impl FunctionSignature {
    pub fn fixed(return_type: DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Fixed(return_type),
            is_aggregate: false,
            is_window: false,
        }
    }

    pub fn with_args(mut self, min: usize, max: Option<usize>) -> Self {
        self.min_args = min;
        self.max_args = max;
        self
    }

    pub fn aggregate(mut self) -> Self {
        self.is_aggregate = true;
        self
    }

    pub fn window(mut self) -> Self {
        self.is_window = true;
        self
    }

    pub fn same_as_arg(arg_index: usize) -> Self {
        Self {
            min_args: arg_index + 1,
            max_args: None,
            return_type: ReturnType::SameAsArg(arg_index),
            is_aggregate: false,
            is_window: false,
        }
    }

    pub fn custom(f: fn(&[DataType]) -> DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Custom(f),
            is_aggregate: false,
            is_window: false,
        }
    }
}

pub struct FunctionRegistry {
    functions: HashMap<String, FunctionSignature>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, sig: FunctionSignature) {
        self.functions.insert(name.to_uppercase(), sig);
    }

    #[allow(dead_code)] // new type inference module, not yet fully integrated
    pub fn get(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_uppercase())
    }

    pub fn resolve_return_type(&self, name: &str, arg_types: &[DataType]) -> Option<DataType> {
        let sig = self.functions.get(&name.to_uppercase())?;
        Some(match &sig.return_type {
            ReturnType::Fixed(dt) => dt.clone(),
            ReturnType::SameAsArg(idx) => arg_types.get(*idx).cloned().unwrap_or(DataType::Text),
            ReturnType::FirstNonNull => arg_types.first().cloned().unwrap_or(DataType::Text),
            ReturnType::NumericPromotion => promote_numeric_types(arg_types),
            ReturnType::Custom(f) => f(arg_types),
        })
    }
}

fn promote_numeric_types(types: &[DataType]) -> DataType {
    let mut result = DataType::Int32;
    for t in types {
        result = match (&result, t) {
            (_, DataType::Float64) | (DataType::Float64, _) => DataType::Float64,
            (_, DataType::Numeric { .. }) | (DataType::Numeric { .. }, _) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            (_, DataType::Int64) | (DataType::Int64, _) => DataType::Int64,
            _ => result,
        };
    }
    result
}

pub fn global_registry() -> &'static FunctionRegistry {
    static REGISTRY: OnceLock<FunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut r = FunctionRegistry::new();
        register_builtin_functions(&mut r);
        r
    })
}

fn register_builtin_functions(r: &mut FunctionRegistry) {
    // Aggregate functions
    r.register(
        "COUNT",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(1))
            .aggregate(),
    );
    r.register(
        "SUM",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Int32) => DataType::Int64,
            Some(DataType::Int64) | Some(DataType::Numeric { .. }) => DataType::Numeric {
                precision: None,
                scale: None,
            },
            Some(DataType::Float64) => DataType::Float64,
            _ => DataType::Numeric {
                precision: None,
                scale: None,
            },
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "AVG",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Float64) => DataType::Float64,
            _ => DataType::Numeric {
                precision: None,
                scale: None,
            },
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "MIN",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "MAX",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "STRING_AGG",
        FunctionSignature::fixed(DataType::Text)
            .with_args(2, Some(2))
            .aggregate(),
    );
    r.register(
        "ARRAY_AGG",
        FunctionSignature::custom(|args| {
            DataType::Array(Box::new(args.first().cloned().unwrap_or(DataType::Text)))
        })
        .with_args(1, Some(1))
        .aggregate(),
    );
    r.register(
        "BOOL_AND",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "BOOL_OR",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "EVERY",
        FunctionSignature::fixed(DataType::Boolean)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "JSONB_AGG",
        FunctionSignature::fixed(DataType::Jsonb)
            .with_args(1, Some(1))
            .aggregate(),
    );
    r.register(
        "JSON_AGG",
        FunctionSignature::fixed(DataType::Json)
            .with_args(1, Some(1))
            .aggregate(),
    );

    // Window functions
    r.register(
        "ROW_NUMBER",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "RANK",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "DENSE_RANK",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "NTILE",
        FunctionSignature::fixed(DataType::Int64)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "LAG",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(3))
            .window(),
    );
    r.register(
        "LEAD",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(3))
            .window(),
    );
    r.register(
        "FIRST_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "LAST_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(1, Some(1))
            .window(),
    );
    r.register(
        "NTH_VALUE",
        FunctionSignature::same_as_arg(0)
            .with_args(2, Some(2))
            .window(),
    );
    r.register(
        "PERCENT_RANK",
        FunctionSignature::fixed(DataType::Float64)
            .with_args(0, Some(0))
            .window(),
    );
    r.register(
        "CUME_DIST",
        FunctionSignature::fixed(DataType::Float64)
            .with_args(0, Some(0))
            .window(),
    );

    // String functions
    r.register(
        "LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHAR_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHARACTER_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "OCTET_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "BIT_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "UPPER",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "LOWER",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "INITCAP",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "TRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "LTRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "RTRIM",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "LPAD",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "RPAD",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "REPEAT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "REVERSE",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "REPLACE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "TRANSLATE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "CONCAT",
        FunctionSignature::fixed(DataType::Text).with_args(1, None),
    );
    r.register(
        "CONCAT_WS",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "LEFT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "RIGHT",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "SUBSTRING",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "SUBSTR",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "SPLIT_PART",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "POSITION",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "STRPOS",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ASCII",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "CHR",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "MD5",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "SHA256",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "ENCODE",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "DECODE",
        FunctionSignature::fixed(DataType::Bytes).with_args(2, Some(2)),
    );
    r.register(
        "FORMAT",
        FunctionSignature::fixed(DataType::Text).with_args(1, None),
    );
    r.register(
        "QUOTE_IDENT",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "QUOTE_LITERAL",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "QUOTE_NULLABLE",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "REGEXP_REPLACE",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(4)),
    );
    r.register(
        "REGEXP_MATCH",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );
    r.register(
        "REGEXP_MATCHES",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );

    // Math functions
    r.register(
        "ABS",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "CEIL",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "CEILING",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "FLOOR",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );
    r.register(
        "ROUND",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "TRUNC",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "TRUNCATE",
        FunctionSignature::same_as_arg(0).with_args(1, Some(2)),
    );
    r.register(
        "MOD",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "POWER",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "POW",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "SQRT",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "CBRT",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "EXP",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "LN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "LOG",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(2)),
    );
    r.register(
        "LOG10",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "SIGN",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "PI",
        FunctionSignature::fixed(DataType::Float64).with_args(0, Some(0)),
    );
    r.register(
        "RANDOM",
        FunctionSignature::fixed(DataType::Float64).with_args(0, Some(0)),
    );
    r.register(
        "DEGREES",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "RADIANS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "SIN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "COS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "TAN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ASIN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ACOS",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ATAN",
        FunctionSignature::fixed(DataType::Float64).with_args(1, Some(1)),
    );
    r.register(
        "ATAN2",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "GREATEST",
        FunctionSignature::same_as_arg(0).with_args(1, None),
    );
    r.register(
        "LEAST",
        FunctionSignature::same_as_arg(0).with_args(1, None),
    );
    r.register(
        "WIDTH_BUCKET",
        FunctionSignature::fixed(DataType::Int32).with_args(4, Some(4)),
    );

    // Date/time functions
    r.register(
        "NOW",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_TIME",
        FunctionSignature::fixed(DataType::Time).with_args(0, Some(0)),
    );
    r.register(
        "LOCALTIME",
        FunctionSignature::fixed(DataType::Time).with_args(0, Some(0)),
    );
    r.register(
        "LOCALTIMESTAMP",
        FunctionSignature::fixed(DataType::Timestamp).with_args(0, Some(0)),
    );
    r.register(
        "DATE",
        FunctionSignature::fixed(DataType::Date).with_args(1, Some(1)),
    );
    r.register(
        "TIME",
        FunctionSignature::fixed(DataType::Time).with_args(1, Some(1)),
    );
    r.register(
        "DATE_TRUNC",
        FunctionSignature::same_as_arg(1).with_args(2, Some(2)),
    );
    r.register(
        "DATE_PART",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "EXTRACT",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "AGE",
        FunctionSignature::fixed(DataType::Interval).with_args(1, Some(2)),
    );
    r.register(
        "TO_CHAR",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "TO_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(2, Some(2)),
    );
    r.register(
        "TO_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(1, Some(2)),
    );
    r.register(
        "TO_NUMBER",
        FunctionSignature::fixed(DataType::Numeric {
            precision: None,
            scale: None,
        })
        .with_args(2, Some(2)),
    );
    r.register(
        "MAKE_DATE",
        FunctionSignature::fixed(DataType::Date).with_args(3, Some(3)),
    );
    r.register(
        "MAKE_TIME",
        FunctionSignature::fixed(DataType::Time).with_args(3, Some(3)),
    );
    r.register(
        "MAKE_TIMESTAMP",
        FunctionSignature::fixed(DataType::Timestamp).with_args(6, Some(6)),
    );
    r.register(
        "MAKE_TIMESTAMPTZ",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(6, Some(7)),
    );
    r.register(
        "MAKE_INTERVAL",
        FunctionSignature::fixed(DataType::Interval).with_args(0, Some(7)),
    );
    r.register(
        "CLOCK_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "STATEMENT_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "TRANSACTION_TIMESTAMP",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "TIMEOFDAY",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );

    // UUID functions
    r.register(
        "GEN_RANDOM_UUID",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );
    r.register(
        "UUID_GENERATE_V4",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );
    r.register(
        "UUIDV7",
        FunctionSignature::fixed(DataType::Uuid).with_args(0, Some(0)),
    );

    // JSON/JSONB functions
    r.register(
        "TO_JSON",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(1)),
    );
    r.register(
        "TO_JSONB",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "ROW_TO_JSON",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(2)),
    );
    r.register(
        "JSON_BUILD_OBJECT",
        FunctionSignature::fixed(DataType::Json).with_args(0, None),
    );
    r.register(
        "JSONB_BUILD_OBJECT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(0, None),
    );
    r.register(
        "JSON_BUILD_ARRAY",
        FunctionSignature::fixed(DataType::Json).with_args(0, None),
    );
    r.register(
        "JSONB_BUILD_ARRAY",
        FunctionSignature::fixed(DataType::Jsonb).with_args(0, None),
    );
    r.register(
        "JSON_OBJECT",
        FunctionSignature::fixed(DataType::Json).with_args(1, Some(2)),
    );
    r.register(
        "JSONB_OBJECT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(2)),
    );
    r.register(
        "JSON_ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "JSON_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSON_EXTRACT_PATH",
        FunctionSignature::fixed(DataType::Json).with_args(2, None),
    );
    r.register(
        "JSONB_EXTRACT_PATH",
        FunctionSignature::fixed(DataType::Jsonb).with_args(2, None),
    );
    r.register(
        "JSON_EXTRACT_PATH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "JSONB_EXTRACT_PATH_TEXT",
        FunctionSignature::fixed(DataType::Text).with_args(2, None),
    );
    r.register(
        "JSONB_SET",
        FunctionSignature::fixed(DataType::Jsonb).with_args(3, Some(4)),
    );
    r.register(
        "JSONB_INSERT",
        FunctionSignature::fixed(DataType::Jsonb).with_args(3, Some(4)),
    );
    r.register(
        "JSONB_PRETTY",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_STRIP_NULLS",
        FunctionSignature::fixed(DataType::Jsonb).with_args(1, Some(1)),
    );
    r.register(
        "JSONB_EXISTS",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_EXISTS_ANY",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_EXISTS_ALL",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_CONTAINS",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );
    r.register(
        "JSONB_CONTAINED",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(2)),
    );

    // Array functions
    r.register(
        "ARRAY_LENGTH",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_DIMS",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "ARRAY_LOWER",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_UPPER",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_NDIMS",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "ARRAY_POSITION",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(3)),
    );
    r.register(
        "ARRAY_POSITIONS",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Int32))).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_CAT",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_APPEND",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_PREPEND",
        FunctionSignature::same_as_arg(1).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_REMOVE",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "ARRAY_REPLACE",
        FunctionSignature::same_as_arg(0).with_args(3, Some(3)),
    );
    r.register(
        "ARRAY_TO_STRING",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(3)),
    );
    r.register(
        "STRING_TO_ARRAY",
        FunctionSignature::fixed(DataType::Array(Box::new(DataType::Text))).with_args(2, Some(3)),
    );
    r.register(
        "UNNEST",
        FunctionSignature::custom(|args| match args.first() {
            Some(DataType::Array(inner)) => inner.as_ref().clone(),
            _ => DataType::Text,
        })
        .with_args(1, Some(1)),
    );

    // Sequence functions
    r.register(
        "NEXTVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "CURRVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "SETVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(2, Some(3)),
    );
    r.register(
        "LASTVAL",
        FunctionSignature::fixed(DataType::Int64).with_args(0, Some(0)),
    );

    // Conditional functions
    r.register(
        "COALESCE",
        FunctionSignature {
            min_args: 1,
            max_args: None,
            return_type: ReturnType::FirstNonNull,
            is_aggregate: false,
            is_window: false,
        },
    );
    r.register(
        "NULLIF",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "IFNULL",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "NVL",
        FunctionSignature::same_as_arg(0).with_args(2, Some(2)),
    );
    r.register(
        "NVL2",
        FunctionSignature::same_as_arg(1).with_args(3, Some(3)),
    );

    // Type conversion (special handling)
    r.register(
        "CAST",
        FunctionSignature::same_as_arg(0).with_args(1, Some(1)),
    );

    // System functions
    r.register(
        "CURRENT_USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_SCHEMA",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_DATABASE",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_CATALOG",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "SESSION_USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "USER",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "PG_TYPEOF",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "VERSION",
        FunctionSignature::fixed(DataType::Text).with_args(0, Some(0)),
    );
    r.register(
        "CURRENT_SETTING",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "SET_CONFIG",
        FunctionSignature::fixed(DataType::Text).with_args(3, Some(3)),
    );
    r.register(
        "PG_BACKEND_PID",
        FunctionSignature::fixed(DataType::Int32).with_args(0, Some(0)),
    );
    r.register(
        "PG_POSTMASTER_START_TIME",
        FunctionSignature::fixed(DataType::TimestampTz).with_args(0, Some(0)),
    );
    r.register(
        "HAS_TABLE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_SCHEMA_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_DATABASE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "PG_TABLE_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_RELATION_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(2)),
    );
    r.register(
        "PG_TOTAL_RELATION_SIZE",
        FunctionSignature::fixed(DataType::Int64).with_args(1, Some(1)),
    );
    r.register(
        "PG_SIZE_PRETTY",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(1)),
    );
    r.register(
        "OBJ_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(1, Some(2)),
    );
    r.register(
        "COL_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );
    r.register(
        "SHOBJ_DESCRIPTION",
        FunctionSignature::fixed(DataType::Text).with_args(2, Some(2)),
    );

    // Bytea functions
    r.register(
        "GET_BYTE",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "SET_BYTE",
        FunctionSignature::fixed(DataType::Bytes).with_args(3, Some(3)),
    );
    r.register(
        "GET_BIT",
        FunctionSignature::fixed(DataType::Int32).with_args(2, Some(2)),
    );
    r.register(
        "SET_BIT",
        FunctionSignature::fixed(DataType::Bytes).with_args(3, Some(3)),
    );
    r.register(
        "INT4SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "INT8SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "UUID_SEND",
        FunctionSignature::fixed(DataType::Bytes).with_args(1, Some(1)),
    );
    r.register(
        "BYTEA_STRING_AGG",
        FunctionSignature::fixed(DataType::Bytes)
            .with_args(2, Some(2))
            .aggregate(),
    );

    // Vector functions (extension)
    r.register(
        "VECTOR_DIMS",
        FunctionSignature::fixed(DataType::Int32).with_args(1, Some(1)),
    );
    r.register(
        "L2_DISTANCE",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "INNER_PRODUCT",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );
    r.register(
        "COSINE_DISTANCE",
        FunctionSignature::fixed(DataType::Float64).with_args(2, Some(2)),
    );

    // Special internal functions
    r.register(
        "GROUPING",
        FunctionSignature::fixed(DataType::Int32).with_args(1, None),
    );

    // pg_catalog visibility functions (return boolean)
    r.register(
        "PG_TABLE_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_FUNCTION_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_TYPE_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "PG_OPERATOR_IS_VISIBLE",
        FunctionSignature::fixed(DataType::Boolean).with_args(1, Some(1)),
    );
    r.register(
        "HAS_SCHEMA_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
    r.register(
        "HAS_TABLE_PRIVILEGE",
        FunctionSignature::fixed(DataType::Boolean).with_args(2, Some(3)),
    );
}
