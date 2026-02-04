use super::SqlFn;
use crate::sql::fts;
use crate::types::Value;
use anyhow::Result;
use std::collections::HashMap;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("TO_TSVECTOR", to_tsvector);
    map.insert("PLAINTO_TSQUERY", plainto_tsquery);
    map.insert("TO_TSQUERY", to_tsquery);
    map.insert("TS_RANK", ts_rank);
    map.insert("TS_RANK_CD", ts_rank);
    map.insert("SETWEIGHT", setweight);
}

fn to_tsvector(args: Vec<Value>) -> Result<Value> {
    fts::to_tsvector(args)
}

fn plainto_tsquery(args: Vec<Value>) -> Result<Value> {
    fts::plainto_tsquery(args)
}

fn to_tsquery(args: Vec<Value>) -> Result<Value> {
    fts::to_tsquery(args)
}

fn ts_rank(args: Vec<Value>) -> Result<Value> {
    fts::ts_rank(args)
}

fn setweight(args: Vec<Value>) -> Result<Value> {
    if args.len() < 2 {
        return Err(anyhow::anyhow!("setweight requires 2 arguments"));
    }

    let tsvector_str = match &args[0] {
        Value::Tsvector(s) => s.as_str(),
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("setweight requires tsvector argument")),
    };

    let weight = match &args[1] {
        Value::Text(s) => s.chars().next().unwrap_or('A'),
        Value::Null => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("setweight requires text weight argument")),
    };

    let result = tsvector_str
        .split_whitespace()
        .map(|entry| {
            if let Some(colon_pos) = entry.rfind(':') {
                let word_part = &entry[..colon_pos];
                let pos_part = &entry[colon_pos + 1..];
                let pos_num: String = pos_part.chars().filter(|c| c.is_ascii_digit()).collect();
                format!("{}:{}{}", word_part, pos_num, weight)
            } else {
                entry.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Value::Tsvector(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_tsvector_registered() {
        let result = to_tsvector(vec![Value::Text("hello world".to_string())]).unwrap();
        assert!(matches!(result, Value::Tsvector(_)));
    }

    #[test]
    fn test_plainto_tsquery_registered() {
        let result = plainto_tsquery(vec![Value::Text("hello world".to_string())]).unwrap();
        assert!(matches!(result, Value::Tsquery(_)));
    }
}
