//! Function signature registry

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::model::DataType;

mod aggregate_window;
mod embedding;
pub(crate) mod http;
mod json;
mod math;
mod misc;
mod string;
mod system;
mod temporal;

#[derive(Clone)]
pub enum ReturnType {
    Fixed(DataType),
    SameAsArg(usize),
    FirstNonNull,
    Custom(fn(&[DataType]) -> DataType),
}

#[derive(Clone)]
pub struct FunctionSignature {
    pub min_args: usize,
    pub max_args: Option<usize>,
    pub return_type: ReturnType,
    pub is_aggregate: bool,
    pub is_window: bool,
    /// Expected argument types, used for parameter coercion in PREPARE.
    /// Empty means no explicit arg type info (coercion handled elsewhere or skipped).
    pub arg_types: Vec<DataType>,
}

impl FunctionSignature {
    pub fn fixed(return_type: DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Fixed(return_type),
            is_aggregate: false,
            is_window: false,
            arg_types: Vec::new(),
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
            arg_types: Vec::new(),
        }
    }

    pub fn custom(f: fn(&[DataType]) -> DataType) -> Self {
        Self {
            min_args: 0,
            max_args: None,
            return_type: ReturnType::Custom(f),
            is_aggregate: false,
            is_window: false,
            arg_types: Vec::new(),
        }
    }

    pub fn with_arg_types(mut self, types: Vec<DataType>) -> Self {
        self.arg_types = types;
        self
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

    pub fn get(&self, name: &str) -> Option<&FunctionSignature> {
        self.functions.get(&name.to_uppercase())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &FunctionSignature)> {
        self.functions
            .iter()
            .map(|(name, sig)| (name.as_str(), sig))
    }

    pub fn resolve_return_type(&self, name: &str, arg_types: &[DataType]) -> Option<DataType> {
        let sig = self.functions.get(&name.to_uppercase())?;
        Some(match &sig.return_type {
            ReturnType::Fixed(dt) => dt.clone(),
            ReturnType::SameAsArg(idx) => arg_types.get(*idx).cloned()?,
            ReturnType::FirstNonNull => arg_types.first().cloned()?,
            ReturnType::Custom(f) => f(arg_types),
        })
    }
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
    aggregate_window::register(r);
    embedding::register(r);
    http::register(r);
    string::register(r);
    math::register(r);
    temporal::register(r);
    json::register(r);
    system::register(r);
    misc::register(r);
}
