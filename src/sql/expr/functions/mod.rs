use crate::model::Value;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::OnceLock;

pub mod array;
pub mod datetime;
pub mod embedding;
pub mod encoding;
pub mod fs9;
pub mod fts;
pub mod http;
pub mod json;
pub mod math;
pub mod misc;
pub mod pg_compat;
pub mod regex;
pub mod string;
pub mod uuid;
pub mod vector;

pub type SqlFn = fn(args: Vec<Value>) -> Result<Value>;

static REGISTRY: OnceLock<HashMap<&'static str, SqlFn>> = OnceLock::new();

fn init_registry() -> HashMap<&'static str, SqlFn> {
    let mut map: HashMap<&'static str, SqlFn> = HashMap::with_capacity(200);

    array::register(&mut map);
    datetime::register(&mut map);
    embedding::register(&mut map);
    encoding::register(&mut map);
    fs9::register(&mut map);
    fts::register(&mut map);
    http::register(&mut map);
    json::register(&mut map);
    math::register(&mut map);
    misc::register(&mut map);
    pg_compat::register(&mut map);
    regex::register(&mut map);
    string::register(&mut map);
    uuid::register(&mut map);
    vector::register(&mut map);

    map
}

pub fn get_registry() -> &'static HashMap<&'static str, SqlFn> {
    REGISTRY.get_or_init(init_registry)
}
