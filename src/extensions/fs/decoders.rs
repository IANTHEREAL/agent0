use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};

use crate::extensions::fs::backend::FsFileInfo;
use crate::model::{ColumnDef, DataType, Row, TableSchema, Value};

pub(crate) struct DecodedRows {
    pub schema: TableSchema,
    pub rows: Vec<Row>,
}

pub(crate) fn detect_format(path: &str, explicit_format: Option<&str>) -> &'static str {
    if let Some(fmt) = explicit_format {
        if fmt.eq_ignore_ascii_case("csv") {
            return "csv";
        }
        if fmt.eq_ignore_ascii_case("tsv") {
            return "tsv";
        }
        if fmt.eq_ignore_ascii_case("jsonl") || fmt.eq_ignore_ascii_case("ndjson") {
            return "jsonl";
        }
        if fmt.eq_ignore_ascii_case("parquet") {
            return "parquet";
        }
        return "text";
    }

    match path.rsplit('.').next() {
        Some(ext) if ext.eq_ignore_ascii_case("csv") => "csv",
        Some(ext) if ext.eq_ignore_ascii_case("tsv") => "tsv",
        Some(ext) if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("ndjson") => {
            "jsonl"
        }
        Some(ext) if ext.eq_ignore_ascii_case("parquet") => "parquet",
        _ => "text",
    }
}

pub(crate) fn decode_raw_text(data: &[u8], path: &str, max_rows: usize) -> DecodedRows {
    let schema = make_schema(
        "fs9",
        vec![
            make_column("_line_number", DataType::Int64, false),
            make_column("line", DataType::Text, false),
            make_column("_path", DataType::Text, false),
        ],
    );

    let text = String::from_utf8_lossy(data);
    let mut rows = Vec::new();

    for (idx, line) in text.lines().enumerate() {
        if rows.len() >= max_rows {
            break;
        }
        rows.push(Row::new(vec![
            Value::Int64((idx + 1) as i64),
            Value::Text(line.to_string()),
            Value::Text(path.to_string()),
        ]));
    }

    DecodedRows { schema, rows }
}

pub(crate) fn decode_directory(entries: Vec<FsFileInfo>) -> DecodedRows {
    let schema = make_schema(
        "fs9",
        vec![
            make_column("path", DataType::Text, false),
            make_column("type", DataType::Text, false),
            make_column("size", DataType::Int64, false),
            make_column("mode", DataType::Int64, false),
            make_column("mtime", DataType::Text, false),
        ],
    );

    let rows = entries
        .into_iter()
        .map(|entry| {
            let mtime = i64::try_from(entry.mtime)
                .ok()
                .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
                .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

            Row::new(vec![
                Value::Text(entry.path),
                Value::Text(
                    if entry.is_symlink {
                        "symlink"
                    } else if entry.is_dir {
                        "dir"
                    } else {
                        "file"
                    }
                    .to_string(),
                ),
                Value::Int64(entry.size as i64),
                Value::Int64(entry.mode as i64),
                Value::Text(mtime),
            ])
        })
        .collect();

    DecodedRows { schema, rows }
}

pub(crate) fn decode_csv(
    data: &[u8],
    path: &str,
    delimiter: Option<char>,
    header: Option<bool>,
    max_rows: usize,
) -> Result<DecodedRows> {
    let delimiter = delimiter.unwrap_or(',');
    let delimiter = u8::try_from(delimiter as u32)
        .map_err(|_| anyhow::anyhow!("delimiter must be a single-byte character"))?;
    let has_headers = header != Some(false);

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .has_headers(has_headers)
        .from_reader(data);

    let path_value = Value::Text(path.to_string());

    if has_headers {
        let headers = reader.headers()?.clone();
        let col_count = headers.len();

        let mut columns = vec![make_column("_line_number", DataType::Int64, false)];
        for name in headers.iter() {
            columns.push(make_column(name, DataType::Text, true));
        }
        columns.push(make_column("_path", DataType::Text, false));
        let schema = make_schema("fs9", columns);

        let mut rows = Vec::new();
        for (idx, record) in reader.records().enumerate() {
            if rows.len() >= max_rows {
                break;
            }
            let record = record?;
            let mut values = Vec::with_capacity(col_count + 2);
            values.push(Value::Int64((idx + 1) as i64));
            for col_idx in 0..col_count {
                match record.get(col_idx) {
                    Some(v) => values.push(Value::Text(v.to_string())),
                    None => values.push(Value::Null),
                }
            }
            values.push(path_value.clone());
            rows.push(Row::new(values));
        }

        return Ok(DecodedRows { schema, rows });
    }

    let mut records = reader.records();
    let Some(first) = records.next() else {
        let schema = make_schema(
            "fs9",
            vec![
                make_column("_line_number", DataType::Int64, false),
                make_column("_path", DataType::Text, false),
            ],
        );
        return Ok(DecodedRows {
            schema,
            rows: vec![],
        });
    };

    let first = first?;
    let col_count = first.len();
    let mut columns = vec![make_column("_line_number", DataType::Int64, false)];
    for idx in 0..col_count {
        columns.push(make_column(&format!("col_{idx}"), DataType::Text, true));
    }
    columns.push(make_column("_path", DataType::Text, false));
    let schema = make_schema("fs9", columns);

    let mut rows = Vec::new();
    if max_rows > 0 {
        let mut values = Vec::with_capacity(col_count + 2);
        values.push(Value::Int64(1));
        for col_idx in 0..col_count {
            match first.get(col_idx) {
                Some(v) => values.push(Value::Text(v.to_string())),
                None => values.push(Value::Null),
            }
        }
        values.push(path_value.clone());
        rows.push(Row::new(values));
    }

    for (idx, record) in records.enumerate() {
        if rows.len() >= max_rows {
            break;
        }
        let record = record?;
        let mut values = Vec::with_capacity(col_count + 2);
        values.push(Value::Int64((idx + 2) as i64));
        for col_idx in 0..col_count {
            match record.get(col_idx) {
                Some(v) => values.push(Value::Text(v.to_string())),
                None => values.push(Value::Null),
            }
        }
        values.push(path_value.clone());
        rows.push(Row::new(values));
    }

    Ok(DecodedRows { schema, rows })
}

pub(crate) fn decode_jsonl(data: &[u8], path: &str, max_rows: usize) -> DecodedRows {
    let schema = make_schema(
        "fs9",
        vec![
            make_column("_line_number", DataType::Int64, false),
            make_column("line", DataType::Jsonb, false),
            make_column("_path", DataType::Text, false),
        ],
    );

    let text = String::from_utf8_lossy(data);
    let mut rows = Vec::new();
    let path_value = Value::Text(path.to_string());

    for (line_num, line) in text.lines().enumerate() {
        if rows.len() >= max_rows {
            break;
        }

        // Skip empty lines
        if line.trim().is_empty() {
            continue;
        }

        // Try to parse as JSON
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(_) => {
                // Valid JSON - store as Jsonb
                rows.push(Row::new(vec![
                    Value::Int64((line_num + 1) as i64),
                    Value::Jsonb(line.to_string()),
                    path_value.clone(),
                ]));
            }
            Err(_) => {
                // Invalid JSON - skip silently
                continue;
            }
        }
    }

    DecodedRows { schema, rows }
}

/// Extract user-visible column names from an fs9 CSV schema,
/// excluding synthetic columns (_line_number, _path).
pub(crate) fn csv_user_column_names(schema: &TableSchema) -> Vec<&str> {
    schema
        .columns
        .iter()
        .filter(|c| c.name != "_line_number" && c.name != "_path")
        .map(|c| c.name.as_str())
        .collect()
}

/// Decode only the CSV header from the given data, returning the schema.
/// Used for glob header validation without reading all rows.
pub(crate) fn decode_csv_header_only(
    data: &[u8],
    path: &str,
    delimiter: Option<char>,
) -> Result<TableSchema> {
    decode_csv(data, path, delimiter, Some(true), 0).map(|d| d.schema)
}

fn make_column(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        generation_expr: None,
        generation_expr_authorized_by: None,
        collation: None,
    }
}

fn make_schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
    TableSchema {
        table_id: 0,
        name: name.to_string(),
        columns,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        version: 1,
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format_csv() {
        assert_eq!(detect_format("file.csv", None), "csv");
        assert_eq!(detect_format("file.CSV", None), "csv");
    }

    #[test]
    fn test_detect_format_tsv() {
        assert_eq!(detect_format("file.tsv", None), "tsv");
    }

    #[test]
    fn test_detect_format_jsonl() {
        assert_eq!(detect_format("file.jsonl", None), "jsonl");
        assert_eq!(detect_format("file.ndjson", None), "jsonl");
    }

    #[test]
    fn test_detect_format_text_fallback() {
        assert_eq!(detect_format("file.txt", None), "text");
        assert_eq!(detect_format("file.dat", None), "text");
        assert_eq!(detect_format("file", None), "text");
    }

    #[test]
    fn test_detect_format_explicit_override() {
        assert_eq!(detect_format("file.txt", Some("csv")), "csv");
        assert_eq!(detect_format("file.csv", Some("jsonl")), "jsonl");
    }

    #[test]
    fn test_detect_parquet_format() {
        assert_eq!(detect_format("data.parquet", None), "parquet");
        assert_eq!(detect_format("DATA.PARQUET", None), "parquet");
        assert_eq!(detect_format("data.Parquet", None), "parquet");
        assert_eq!(detect_format("data.parquet", Some("csv")), "csv"); // explicit format overrides
        assert_eq!(detect_format("data.csv", Some("parquet")), "parquet"); // explicit parquet
    }

    #[test]
    fn test_decode_raw_text_basic() {
        let data = b"line one\nline two\nline three";
        let decoded = decode_raw_text(data, "/tmp/test.txt", 100);
        assert_eq!(decoded.rows.len(), 3);
        assert_eq!(decoded.rows[0].values[0], Value::Int64(1));
        assert_eq!(
            decoded.rows[0].values[1],
            Value::Text("line one".to_string())
        );
        assert_eq!(
            decoded.rows[0].values[2],
            Value::Text("/tmp/test.txt".to_string())
        );
    }

    #[test]
    fn test_decode_raw_text_empty() {
        let data = b"";
        let decoded = decode_raw_text(data, "/tmp/empty.txt", 100);
        assert_eq!(decoded.rows.len(), 0);
    }

    #[test]
    fn test_decode_raw_text_trailing_newline() {
        let data = b"line one\nline two\n";
        let decoded = decode_raw_text(data, "/tmp/test.txt", 100);
        assert_eq!(decoded.rows.len(), 2);
    }

    #[test]
    fn test_decode_raw_text_max_rows() {
        let data = b"a\nb\nc\nd\ne";
        let decoded = decode_raw_text(data, "/tmp/test.txt", 3);
        assert_eq!(decoded.rows.len(), 3);
    }

    #[test]
    fn test_decode_csv_with_header() {
        let data = b"name,age,city\nAlice,30,Beijing\nBob,25,Shanghai";
        let decoded = decode_csv(data, "/tmp/test.csv", None, None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.schema.columns.len(), 5);
        assert_eq!(decoded.schema.columns[1].name, "name");
        assert_eq!(decoded.schema.columns[2].name, "age");
        assert_eq!(decoded.schema.columns[3].name, "city");
        assert_eq!(decoded.rows[0].values[0], Value::Int64(1));
        assert_eq!(decoded.rows[0].values[1], Value::Text("Alice".to_string()));
    }

    #[test]
    fn test_decode_csv_without_header() {
        let data = b"Alice,30,Beijing\nBob,25,Shanghai";
        let decoded = decode_csv(data, "/tmp/test.csv", None, Some(false), 100).unwrap();
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.schema.columns[1].name, "col_0");
        assert_eq!(decoded.schema.columns[2].name, "col_1");
        assert_eq!(decoded.schema.columns[3].name, "col_2");
    }

    #[test]
    fn test_decode_csv_tsv_delimiter() {
        let data = b"name\tage\nAlice\t30";
        let decoded = decode_csv(data, "/tmp/test.tsv", Some('\t'), None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 1);
        assert_eq!(decoded.schema.columns[1].name, "name");
        assert_eq!(decoded.rows[0].values[2], Value::Text("30".to_string()));
    }

    #[test]
    fn test_decode_csv_empty() {
        let data = b"";
        let decoded = decode_csv(data, "/tmp/empty.csv", None, None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 0);
        assert!(decoded.schema.columns.len() >= 2);
    }

    #[test]
    fn test_decode_csv_header_only() {
        let data = b"name,age,city\n";
        let decoded = decode_csv(data, "/tmp/test.csv", None, None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 0);
        assert_eq!(decoded.schema.columns.len(), 5);
    }

    #[test]
    fn test_decode_csv_fewer_columns() {
        let data = b"name,age,city\nAlice,30\nBob,25,Shanghai";
        let decoded = decode_csv(data, "/tmp/test.csv", None, None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].values[3], Value::Null);
    }

    #[test]
    fn test_decode_csv_quoted_fields() {
        let data = b"name,description\nAlice,\"has, comma\"\nBob,\"has\nnewline\"";
        let decoded = decode_csv(data, "/tmp/test.csv", None, None, 100).unwrap();
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(
            decoded.rows[0].values[2],
            Value::Text("has, comma".to_string())
        );
    }

    #[test]
    fn test_decode_csv_max_rows() {
        let data = b"name\na\nb\nc\nd\ne";
        let decoded = decode_csv(data, "/tmp/test.csv", None, None, 3).unwrap();
        assert_eq!(decoded.rows.len(), 3);
    }

    #[test]
    fn test_decode_directory() {
        let entries = vec![
            FsFileInfo {
                path: "/tmp/a.txt".to_string(),
                is_dir: false,
                is_symlink: false,
                size: 100,
                mode: 0o644,
                mtime: 1705312200,
                storage: None,
                sealed: None,
            },
            FsFileInfo {
                path: "/tmp/subdir".to_string(),
                is_dir: true,
                is_symlink: false,
                size: 0,
                mode: 0o755,
                mtime: 1705312200,
                storage: None,
                sealed: None,
            },
        ];
        let decoded = decode_directory(entries);
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].values[1], Value::Text("file".to_string()));
        assert_eq!(decoded.rows[1].values[1], Value::Text("dir".to_string()));
    }

    #[test]
    fn test_decode_directory_emits_symlink_type() {
        let decoded = decode_directory(vec![FsFileInfo {
            path: "/tmp/link".to_string(),
            is_dir: false,
            is_symlink: true,
            size: 15,
            mode: 0o777,
            mtime: 1705312200,
            storage: None,
            sealed: Some(false),
        }]);

        assert_eq!(decoded.rows.len(), 1);
        assert_eq!(
            decoded.rows[0].values[1],
            Value::Text("symlink".to_string())
        );
    }

    #[test]
    fn test_decode_raw_text_schema() {
        let decoded = decode_raw_text(b"hello", "/tmp/test.txt", 100);
        assert_eq!(decoded.schema.columns.len(), 3);
        assert_eq!(decoded.schema.columns[0].name, "_line_number");
        assert_eq!(decoded.schema.columns[1].name, "line");
        assert_eq!(decoded.schema.columns[2].name, "_path");
    }

    #[test]
    fn test_decode_directory_schema() {
        let decoded = decode_directory(vec![]);
        assert_eq!(decoded.schema.columns.len(), 5);
        assert_eq!(decoded.schema.columns[0].name, "path");
        assert_eq!(decoded.schema.columns[1].name, "type");
        assert_eq!(decoded.schema.columns[2].name, "size");
        assert_eq!(decoded.schema.columns[3].name, "mode");
        assert_eq!(decoded.schema.columns[4].name, "mtime");
    }

    #[test]
    fn test_decode_jsonl_basic() {
        let data =
            b"{\"level\":\"INFO\",\"msg\":\"started\"}\n{\"level\":\"ERROR\",\"msg\":\"failed\"}";
        let decoded = decode_jsonl(data, "/tmp/logs.jsonl", 100);
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].values[0], Value::Int64(1));
        match &decoded.rows[0].values[1] {
            Value::Jsonb(_) => {}
            other => panic!("Expected Jsonb, got {:?}", other),
        }
        assert_eq!(
            decoded.rows[0].values[2],
            Value::Text("/tmp/logs.jsonl".to_string())
        );
    }

    #[test]
    fn test_decode_jsonl_skip_empty_lines() {
        let data = b"{\"a\":1}\n\n{\"b\":2}\n";
        let decoded = decode_jsonl(data, "/tmp/test.jsonl", 100);
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].values[0], Value::Int64(1));
        assert_eq!(decoded.rows[1].values[0], Value::Int64(3));
    }

    #[test]
    fn test_decode_jsonl_skip_invalid_json() {
        let data = b"{\"valid\":true}\nnot json\n{\"also\":\"valid\"}";
        let decoded = decode_jsonl(data, "/tmp/test.jsonl", 100);
        assert_eq!(decoded.rows.len(), 2);
        assert_eq!(decoded.rows[0].values[0], Value::Int64(1));
        assert_eq!(decoded.rows[1].values[0], Value::Int64(3));
    }

    #[test]
    fn test_decode_jsonl_empty() {
        let decoded = decode_jsonl(b"", "/tmp/empty.jsonl", 100);
        assert_eq!(decoded.rows.len(), 0);
    }

    #[test]
    fn test_decode_jsonl_max_rows() {
        let data = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n{\"d\":4}\n{\"e\":5}";
        let decoded = decode_jsonl(data, "/tmp/test.jsonl", 3);
        assert_eq!(decoded.rows.len(), 3);
    }

    #[test]
    fn test_decode_jsonl_schema() {
        let decoded = decode_jsonl(b"{\"x\":1}", "/tmp/test.jsonl", 100);
        assert_eq!(decoded.schema.columns.len(), 3);
        assert_eq!(decoded.schema.columns[0].name, "_line_number");
        assert_eq!(decoded.schema.columns[0].data_type, DataType::Int64);
        assert_eq!(decoded.schema.columns[1].name, "line");
        assert_eq!(decoded.schema.columns[1].data_type, DataType::Jsonb);
        assert_eq!(decoded.schema.columns[2].name, "_path");
        assert_eq!(decoded.schema.columns[2].data_type, DataType::Text);
    }

    #[test]
    fn test_decode_jsonl_mixed_types() {
        let data = b"{\"obj\":true}\n[1,2,3]\n42\n\"string\"";
        let decoded = decode_jsonl(data, "/tmp/test.jsonl", 100);
        assert_eq!(decoded.rows.len(), 4);
    }
}
