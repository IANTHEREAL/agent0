use anyhow::Result;
use tokio::io::AsyncBufRead;

use crate::types::{Row, TableSchema};

pub(crate) struct StreamingTextDecoder {
    _private: (),
}

impl StreamingTextDecoder {
    pub(crate) fn new(_reader: Box<dyn AsyncBufRead + Unpin + Send>, _path: String) -> Self {
        todo!("StreamingTextDecoder::new")
    }

    pub(crate) fn schema(&self) -> &TableSchema {
        todo!("StreamingTextDecoder::schema")
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<Row>> {
        todo!("StreamingTextDecoder::next_row")
    }

    pub(crate) fn bytes_read(&self) -> usize {
        todo!("StreamingTextDecoder::bytes_read")
    }
}

pub(crate) struct StreamingJsonlDecoder {
    _private: (),
}

impl StreamingJsonlDecoder {
    pub(crate) fn new(_reader: Box<dyn AsyncBufRead + Unpin + Send>, _path: String) -> Self {
        todo!("StreamingJsonlDecoder::new")
    }

    pub(crate) fn schema(&self) -> &TableSchema {
        todo!("StreamingJsonlDecoder::schema")
    }

    pub(crate) async fn next_row(&mut self) -> Result<Option<Row>> {
        todo!("StreamingJsonlDecoder::next_row")
    }

    pub(crate) fn bytes_read(&self) -> usize {
        todo!("StreamingJsonlDecoder::bytes_read")
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
