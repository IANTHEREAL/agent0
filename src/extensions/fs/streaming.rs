use anyhow::Result;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};

fn make_column(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
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
    }
}

fn text_schema() -> TableSchema {
    make_schema(
        "fs9",
        vec![
            make_column("_line_number", DataType::Int64, false),
            make_column("line", DataType::Text, false),
            make_column("_path", DataType::Text, false),
        ],
    )
}

fn jsonl_schema() -> TableSchema {
    make_schema(
        "fs9",
        vec![
            make_column("_line_number", DataType::Int64, false),
            make_column("line", DataType::Jsonb, false),
            make_column("_path", DataType::Text, false),
        ],
    )
}

pub(crate) struct StreamingTextDecoder {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    path: String,
    schema: TableSchema,
    line_number: usize,
    total_bytes: usize,
    buf: String,
}

impl StreamingTextDecoder {
    pub(crate) fn new(reader: Box<dyn AsyncBufRead + Unpin + Send>, path: String) -> Self {
        Self {
            reader,
            path,
            schema: text_schema(),
            line_number: 0,
            total_bytes: 0,
            buf: String::new(),
        }
    }

    pub(crate) fn schema(&self) -> &TableSchema {
        &self.schema
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<Row>> {
        self.buf.clear();
        let n = self.reader.read_line(&mut self.buf).await?;
        if n == 0 {
            return Ok(None);
        }
        self.total_bytes += n;
        self.line_number += 1;

        let line = self.buf.trim_end_matches('\n').trim_end_matches('\r');
        Ok(Some(Row::new(vec![
            Value::Int64(self.line_number as i64),
            Value::Text(line.to_string()),
            Value::Text(self.path.clone()),
        ])))
    }

    pub(crate) fn bytes_read(&self) -> usize {
        self.total_bytes
    }
}

pub(crate) struct StreamingJsonlDecoder {
    reader: Box<dyn AsyncBufRead + Unpin + Send>,
    path: String,
    schema: TableSchema,
    line_number: usize,
    total_bytes: usize,
    buf: String,
}

impl StreamingJsonlDecoder {
    pub(crate) fn new(reader: Box<dyn AsyncBufRead + Unpin + Send>, path: String) -> Self {
        Self {
            reader,
            path,
            schema: jsonl_schema(),
            line_number: 0,
            total_bytes: 0,
            buf: String::new(),
        }
    }

    pub(crate) fn schema(&self) -> &TableSchema {
        &self.schema
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<Row>> {
        loop {
            self.buf.clear();
            let n = self.reader.read_line(&mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }
            self.total_bytes += n;
            self.line_number += 1;

            let trimmed = self.buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            if serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
                continue;
            }

            return Ok(Some(Row::new(vec![
                Value::Int64(self.line_number as i64),
                Value::Jsonb(trimmed.to_string()),
                Value::Text(self.path.clone()),
            ])));
        }
    }

    pub(crate) fn bytes_read(&self) -> usize {
        self.total_bytes
    }
}

pub(crate) struct StreamingCsvDecoder {
    _private: (),
}

impl StreamingCsvDecoder {
    pub(crate) async fn new(
        _reader: Box<dyn AsyncBufRead + Unpin + Send>,
        _path: String,
        _delimiter: Option<char>,
        _has_headers: bool,
    ) -> Result<Self> {
        todo!("StreamingCsvDecoder::new")
    }

    pub(crate) fn schema(&self) -> &TableSchema {
        todo!("StreamingCsvDecoder::schema")
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<Row>> {
        todo!("StreamingCsvDecoder::next_row")
    }

    pub(crate) fn bytes_read(&self) -> usize {
        todo!("StreamingCsvDecoder::bytes_read")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::BufReader;

    use super::*;
    use crate::extensions::fs::decoders;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/pgtikv-fs9-streaming-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    async fn open_buf_reader(path: &str) -> Box<dyn AsyncBufRead + Unpin + Send> {
        let file = tokio::fs::File::open(path).await.expect("open file");
        Box::new(BufReader::new(file))
    }

    #[tokio::test]
    async fn test_streaming_text_yields_same_as_batch() {
        let dir = unique_base("text-same-batch");
        let file = dir.join("lines.txt");
        fs::write(&file, "alpha\nbeta\ngamma\ndelta\nepsilon\n").expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingTextDecoder::new(reader, path_str.clone());

        let batch = decoders::decode_raw_text(
            b"alpha\nbeta\ngamma\ndelta\nepsilon\n",
            &path_str,
            usize::MAX,
        );

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(streaming_rows.len(), batch.rows.len());
        for (s, b) in streaming_rows.iter().zip(batch.rows.iter()) {
            assert_eq!(s.values, b.values);
        }

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_text_empty_file() {
        let dir = unique_base("text-empty");
        let file = dir.join("empty.txt");
        fs::write(&file, "").expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingTextDecoder::new(reader, path_str);

        let row = dec.next_row().await.expect("next_row");
        assert!(row.is_none(), "empty file should yield None immediately");

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_text_tracks_bytes() {
        let dir = unique_base("text-bytes");
        let file = dir.join("bytes.txt");
        let content = "line one\nline two\nline three\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingTextDecoder::new(reader, path_str);

        while dec.next_row().await.expect("next_row").is_some() {}
        assert_eq!(dec.bytes_read(), content.len());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_jsonl_yields_same_as_batch() {
        let dir = unique_base("jsonl-same-batch");
        let file = dir.join("data.jsonl");
        let content = "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingJsonlDecoder::new(reader, path_str.clone());

        let batch = decoders::decode_jsonl(content.as_bytes(), &path_str, usize::MAX);

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(streaming_rows.len(), batch.rows.len());
        for (s, b) in streaming_rows.iter().zip(batch.rows.iter()) {
            assert_eq!(s.values, b.values);
        }

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_jsonl_skips_invalid_and_empty() {
        let dir = unique_base("jsonl-skip");
        let file = dir.join("mixed.jsonl");
        let content = "{\"ok\":1}\nnot json\n\n{\"ok\":2}\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingJsonlDecoder::new(reader, path_str);

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(
            streaming_rows.len(),
            2,
            "should skip invalid and empty lines"
        );

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_csv_yields_same_as_batch() {
        let dir = unique_base("csv-same-batch");
        let file = dir.join("data.csv");
        let content = "name,age,city\nAlice,30,Beijing\nBob,25,Shanghai\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingCsvDecoder::new(reader, path_str.clone(), None, true)
            .await
            .expect("new csv decoder");

        let batch = decoders::decode_csv(content.as_bytes(), &path_str, None, None, usize::MAX)
            .expect("batch decode");

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(streaming_rows.len(), batch.rows.len());
        for (s, b) in streaming_rows.iter().zip(batch.rows.iter()) {
            assert_eq!(s.values, b.values);
        }

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_csv_without_headers() {
        let dir = unique_base("csv-no-header");
        let file = dir.join("noheader.csv");
        let content = "Alice,30,Beijing\nBob,25,Shanghai\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingCsvDecoder::new(reader, path_str.clone(), None, false)
            .await
            .expect("new csv decoder");

        let schema = dec.schema();
        assert!(schema.columns.iter().any(|c| c.name == "col_0"));

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(streaming_rows.len(), 2);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_csv_custom_delimiter() {
        let dir = unique_base("csv-tab-delim");
        let file = dir.join("data.tsv");
        let content = "name\tage\nAlice\t30\nBob\t25\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingCsvDecoder::new(reader, path_str.clone(), Some('\t'), true)
            .await
            .expect("new csv decoder");

        let batch =
            decoders::decode_csv(content.as_bytes(), &path_str, Some('\t'), None, usize::MAX)
                .expect("batch decode");

        let mut streaming_rows = Vec::new();
        while let Some(row) = dec.next_row().await.expect("next_row") {
            streaming_rows.push(row);
        }
        assert_eq!(streaming_rows.len(), batch.rows.len());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_streaming_bytes_counter_cumulative() {
        let dir = unique_base("bytes-cumul");
        let file = dir.join("cumul.txt");
        let content = "short\nmedium line\na somewhat longer line here\n";
        fs::write(&file, content).expect("write");
        let path_str = file.to_string_lossy().to_string();

        let reader = open_buf_reader(&path_str).await;
        let mut dec = StreamingTextDecoder::new(reader, path_str);

        let mut prev_bytes = 0usize;
        while dec.next_row().await.expect("next_row").is_some() {
            let cur = dec.bytes_read();
            assert!(cur > prev_bytes, "bytes_read must increase after each row");
            prev_bytes = cur;
        }
        assert!(prev_bytes > 0, "should have read some bytes");

        cleanup(&dir);
    }
}
