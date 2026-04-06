use std::pin::Pin;

use anyhow::{anyhow, Result};
use arrow_array::RecordBatch;
use arrow_schema::Schema as ArrowSchema;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use parquet::arrow::async_reader::{AsyncFileReader, ParquetRecordBatchStreamBuilder};

use super::http_reader::{build_pinned_parquet_client, fetch_parquet_file_size, HttpParquetReader};
use super::types::{arrow_array_to_value, arrow_type_to_pg_type};
use crate::model::{ColumnDef, TableSchema, Value};

const DEFAULT_BATCH_SIZE: usize = 8192;

type RowStream = Pin<Box<dyn Stream<Item = Result<Vec<Value>>> + Send>>;
type BatchStream = Pin<Box<dyn Stream<Item = Result<Vec<Vec<Value>>>> + Send>>;

fn wrap_parquet_open_error(e: parquet::errors::ParquetError) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("not a valid Parquet file") || msg.contains("magic") || msg.contains("Magic") {
        anyhow!(
            "Not a valid Parquet file (expected PAR1 magic bytes): {}",
            msg
        )
    } else if msg.contains("metadata") || msg.contains("footer") || msg.contains("Footer") {
        anyhow!("Corrupt Parquet file (cannot read metadata): {}", msg)
    } else {
        anyhow!("Failed to open Parquet file: {}", msg)
    }
}

async fn create_fs9_reader(url: &str) -> Result<super::fs9_reader::Fs9ParquetReader> {
    let path = strip_fs9_scheme(url);
    let tenant = crate::extensions::context::tenant_keyspace().ok_or_else(|| {
        anyhow!("read_parquet: tenant keyspace not available in extension context")
    })?;
    if !crate::extensions::fs::backend::is_backend_available() {
        anyhow::bail!("fs9: TiKV storage backend not available");
    }
    if !crate::extensions::context::is_superuser() {
        anyhow::bail!("fs9: permission denied");
    }
    let backend = crate::extensions::fs::backend::acquire_statement_backend(&tenant).await?;
    let data = backend
        .read_file(path, super::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES)
        .await
        .map_err(|e| anyhow!("read_parquet: failed to read fs9 file '{}': {}", path, e))?;
    Ok(super::fs9_reader::Fs9ParquetReader::new(Bytes::from(data)))
}

pub(crate) async fn infer_schema(url: &str) -> Result<TableSchema> {
    if is_fs9_url(url) {
        let reader = create_fs9_reader(url).await?;
        return infer_schema_from_reader(reader).await;
    }

    super::http_reader::validate_parquet_url(url)?;
    let validated = super::http_reader::validate_parquet_url_security(url).await?;
    let client =
        build_pinned_parquet_client(&validated.host, validated.resolved_ip, validated.port)?;
    let file_size = fetch_parquet_file_size(&client, url)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let reader = HttpParquetReader::new(client, url.to_string(), file_size);
    infer_schema_from_reader(reader).await
}

pub(crate) async fn open_row_stream(url: &str) -> Result<(TableSchema, RowStream)> {
    if is_fs9_url(url) {
        let reader = create_fs9_reader(url).await?;
        return open_row_stream_from_reader(reader, DEFAULT_BATCH_SIZE).await;
    }

    super::http_reader::validate_parquet_url(url)?;
    let validated = super::http_reader::validate_parquet_url_security(url).await?;
    let client =
        build_pinned_parquet_client(&validated.host, validated.resolved_ip, validated.port)?;
    let file_size = fetch_parquet_file_size(&client, url)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let reader = HttpParquetReader::new(client, url.to_string(), file_size);
    open_row_stream_from_reader(reader, DEFAULT_BATCH_SIZE).await
}

pub(crate) async fn open_batch_stream(url: &str) -> Result<(TableSchema, BatchStream)> {
    if is_fs9_url(url) {
        let reader = create_fs9_reader(url).await?;
        return open_batch_stream_from_reader(reader, DEFAULT_BATCH_SIZE).await;
    }

    super::http_reader::validate_parquet_url(url)?;
    let validated = super::http_reader::validate_parquet_url_security(url).await?;
    let client =
        build_pinned_parquet_client(&validated.host, validated.resolved_ip, validated.port)?;
    let file_size = fetch_parquet_file_size(&client, url)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let reader = HttpParquetReader::new(client, url.to_string(), file_size);
    open_batch_stream_from_reader(reader, DEFAULT_BATCH_SIZE).await
}

pub(crate) async fn infer_schema_from_reader<R>(reader: R) -> Result<TableSchema>
where
    R: AsyncFileReader + Send + 'static,
{
    let builder = ParquetRecordBatchStreamBuilder::new(reader)
        .await
        .map_err(wrap_parquet_open_error)?;
    arrow_schema_to_table_schema(builder.schema().as_ref())
}

pub(crate) async fn open_batch_stream_from_reader<R>(
    reader: R,
    batch_size: usize,
) -> Result<(TableSchema, BatchStream)>
where
    R: AsyncFileReader + Send + Unpin + 'static,
{
    let builder = ParquetRecordBatchStreamBuilder::new(reader)
        .await
        .map_err(wrap_parquet_open_error)?;
    let schema = arrow_schema_to_table_schema(builder.schema().as_ref())?;

    let stream = builder
        .with_batch_size(batch_size)
        .build()
        .map_err(|e| anyhow!("Failed to create Parquet reader: {}", e))?
        .map(|batch_res| {
            let batch = batch_res.map_err(|e| anyhow!("Error reading Parquet row group: {}", e))?;
            record_batch_to_rows(&batch)
        })
        .boxed();

    Ok((schema, Box::pin(stream)))
}

pub(crate) async fn open_row_stream_from_reader<R>(
    reader: R,
    batch_size: usize,
) -> Result<(TableSchema, RowStream)>
where
    R: AsyncFileReader + Send + Unpin + 'static,
{
    let (schema, batch_stream) = open_batch_stream_from_reader(reader, batch_size).await?;

    let row_stream = batch_stream
        .flat_map(|batch_result| -> BoxStream<'static, Result<Vec<Value>>> {
            match batch_result {
                Ok(rows) => stream::iter(rows.into_iter().map(Ok)).boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            }
        })
        .boxed();

    Ok((schema, Box::pin(row_stream)))
}

pub(crate) fn arrow_schema_to_table_schema(arrow_schema: &ArrowSchema) -> Result<TableSchema> {
    let columns = arrow_schema
        .fields()
        .iter()
        .map(|field| {
            Ok(ColumnDef::new(
                field.name().clone(),
                arrow_type_to_pg_type(field.data_type())?,
                field.is_nullable(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(TableSchema::new(String::new(), 0, columns, Vec::new()))
}

pub(crate) fn record_batch_to_rows(batch: &RecordBatch) -> Result<Vec<Vec<Value>>> {
    let num_rows = batch.num_rows();
    let num_cols = batch.num_columns();
    let mut rows = Vec::with_capacity(num_rows);

    for row_idx in 0..num_rows {
        let mut row = Vec::with_capacity(num_cols);
        for col_idx in 0..num_cols {
            row.push(arrow_array_to_value(
                batch.column(col_idx).as_ref(),
                row_idx,
            )?);
        }
        rows.push(row);
    }

    Ok(rows)
}

/// Returns true if the URL uses the fs9:// scheme.
pub(crate) fn is_fs9_url(url: &str) -> bool {
    url.to_ascii_lowercase().starts_with("fs9://")
}

/// Strip the fs9:// scheme prefix and return the bare filesystem path.
/// "fs9:///absolute/path" → "/absolute/path"
/// "fs9://relative/path" → "relative/path"
pub(crate) fn strip_fs9_scheme(url: &str) -> &str {
    if url.len() >= 6 && url[..6].eq_ignore_ascii_case("fs9://") {
        &url[6..]
    } else {
        url
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use crate::extensions::fs::backend::{
        FsBackend, FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
        FsPresignedRequest, FsStorage, FsWriteStream, FsWriteStreamOptions,
    };
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::ops::Range;
    use std::sync::Arc;

    use futures::future::BoxFuture;
    use futures::FutureExt;
    use parquet::errors::{ParquetError, Result as ParquetResult};
    use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};

    use arrow_array::{ArrayRef, Int32Array, StringArray};
    use arrow_schema::{DataType as ArrowDataType, Field};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
    use tokio::io::{AsyncBufRead, BufReader};

    use crate::model::DataType;

    struct InMemoryParquetReader {
        data: Bytes,
    }

    impl InMemoryParquetReader {
        fn new(data: Bytes) -> Self {
            Self { data }
        }
    }

    impl AsyncFileReader for InMemoryParquetReader {
        fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, ParquetResult<Bytes>> {
            let data = self.data.clone();
            async move {
                let start = usize::try_from(range.start).map_err(|e| {
                    ParquetError::General(format!("range start conversion failed: {e}"))
                })?;
                let end = usize::try_from(range.end).map_err(|e| {
                    ParquetError::General(format!("range end conversion failed: {e}"))
                })?;
                if start > end || end > data.len() {
                    return Err(ParquetError::General(format!(
                        "invalid byte range {start}..{end} for data length {}",
                        data.len()
                    )));
                }
                Ok(data.slice(start..end))
            }
            .boxed()
        }

        fn get_metadata<'a>(
            &'a mut self,
            _options: Option<&'a parquet::arrow::arrow_reader::ArrowReaderOptions>,
        ) -> BoxFuture<'a, ParquetResult<Arc<ParquetMetaData>>> {
            async move {
                let file_size = self.data.len() as u64;
                let metadata = ParquetMetaDataReader::new()
                    .load_and_finish(&mut *self, file_size)
                    .await?;
                Ok(Arc::new(metadata))
            }
            .boxed()
        }
    }

    fn make_test_parquet(num_rows: usize) -> Result<Bytes> {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Int32, false),
            Field::new("name", ArrowDataType::Utf8, true),
        ]));

        let ids: Vec<i32> = (0..num_rows).map(|i| i as i32).collect();
        let names: Vec<Option<String>> = (0..num_rows).map(|i| Some(format!("row_{i}"))).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(ids)) as ArrayRef,
                Arc::new(StringArray::from(names)) as ArrayRef,
            ],
        )
        .map_err(|e| anyhow!("{e}"))?;

        let props = WriterProperties::builder()
            .set_max_row_group_size(5)
            .build();
        let mut out = Vec::new();
        {
            let mut writer =
                ArrowWriter::try_new(&mut out, schema, Some(props)).map_err(|e| anyhow!("{e}"))?;
            writer.write(&batch).map_err(|e| anyhow!("{e}"))?;
            writer.close().map_err(|e| anyhow!("{e}"))?;
        }

        Ok(Bytes::from(out))
    }

    struct MockFsWriteStream;

    #[async_trait]
    impl FsWriteStream for MockFsWriteStream {
        async fn write_chunk(&mut self, _chunk: &[u8]) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn finish(self: Box<Self>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort(self: Box<Self>) -> Result<()> {
            anyhow::bail!("not implemented")
        }
    }

    struct MockFsBackend {
        files: Mutex<HashMap<String, Bytes>>,
    }

    impl MockFsBackend {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }

        fn insert_file(&self, path: &str, data: Bytes) {
            self.files.lock().insert(path.to_string(), data);
        }
    }

    #[async_trait]
    impl FsBackend for MockFsBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            let files = self.files.lock();
            let data = files
                .get(path)
                .ok_or_else(|| anyhow!("missing test file: {path}"))?;
            Ok(FsFileInfo {
                path: path.to_string(),
                is_dir: false,
                is_symlink: false,
                size: data.len() as u64,
                mode: 0o644,
                generation: 1,
                mtime: 0,
                storage: Some(FsStorage::Inline),
                sealed: Some(false),
            })
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            anyhow::bail!("not implemented")
        }

        async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
            let files = self.files.lock();
            let data = files
                .get(path)
                .ok_or_else(|| anyhow!("missing test file: {path}"))?;
            if data.len() > max_bytes {
                anyhow::bail!("file too large for test backend")
            }
            Ok(data.to_vec())
        }

        async fn read_file_stream(
            &self,
            path: &str,
            max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            let data = self.read_file(path, max_bytes).await?;
            Ok(Box::new(BufReader::new(std::io::Cursor::new(data))))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            anyhow::bail!("not implemented")
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn batch_write(
            &self,
            _files: Vec<crate::extensions::fs::backend::FsBatchWriteFile>,
        ) -> Result<Vec<crate::extensions::fs::backend::FsBatchWriteEntry>> {
            anyhow::bail!("not implemented")
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            Ok(Box::new(MockFsWriteStream))
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            anyhow::bail!("not implemented")
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            anyhow::bail!("not implemented")
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            anyhow::bail!("not implemented")
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            anyhow::bail!("not implemented")
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            anyhow::bail!("not implemented")
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[tokio::test]
    async fn test_schema_inference() {
        let data = make_test_parquet(10).expect("create test parquet");
        let reader = InMemoryParquetReader::new(data);

        let schema = infer_schema_from_reader(reader)
            .await
            .expect("infer schema should succeed");

        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[0].data_type, DataType::Int32);
        assert!(!schema.columns[0].nullable);
        assert_eq!(schema.columns[1].name, "name");
        assert_eq!(schema.columns[1].data_type, DataType::Text);
        assert!(schema.columns[1].nullable);
    }

    #[tokio::test]
    async fn test_row_streaming() {
        let data = make_test_parquet(20).expect("create test parquet");
        let reader = InMemoryParquetReader::new(data);

        let (_schema, mut stream) = open_row_stream_from_reader(reader, 4)
            .await
            .expect("open row stream should succeed");

        let mut rows = Vec::new();
        while let Some(row) = stream.next().await {
            rows.push(row.expect("row conversion should succeed"));
        }

        assert_eq!(rows.len(), 20);
        assert_eq!(
            rows[0],
            vec![Value::Int32(0), Value::Text("row_0".to_string())]
        );
        assert_eq!(
            rows[19],
            vec![Value::Int32(19), Value::Text("row_19".to_string())]
        );
    }

    #[tokio::test]
    async fn test_batch_streaming() {
        let data = make_test_parquet(20).expect("create test parquet");
        let reader = InMemoryParquetReader::new(data);

        let (_schema, mut stream) = open_batch_stream_from_reader(reader, 4)
            .await
            .expect("open batch stream should succeed");

        let mut total_rows = 0;
        while let Some(batch) = stream.next().await {
            total_rows += batch.expect("batch conversion should succeed").len();
        }

        assert_eq!(total_rows, 20);
    }

    #[tokio::test]
    async fn test_empty_parquet() {
        let data = make_test_parquet(0).expect("create empty parquet");
        let reader = InMemoryParquetReader::new(data);

        let (schema, mut stream) = open_row_stream_from_reader(reader, 4)
            .await
            .expect("open row stream should succeed");

        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[1].name, "name");

        let mut total_rows = 0;
        while let Some(row) = stream.next().await {
            row.expect("row conversion should succeed");
            total_rows += 1;
        }
        assert_eq!(total_rows, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_infer_schema_fs9_url_uses_cached_backend_without_tikv() {
        let parquet = make_test_parquet(2).expect("create test parquet");
        let backend = Arc::new(MockFsBackend::new());
        backend.insert_file("/cached.parquet", parquet);

        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let schema = infer_schema("fs9:///cached.parquet")
                .await
                .expect("infer schema via cached backend");
            assert_eq!(schema.columns.len(), 2);
            assert_eq!(schema.columns[0].name, "id");
            assert_eq!(schema.columns[1].name, "name");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_open_row_stream_fs9_url_uses_cached_backend_without_tikv() {
        let parquet = make_test_parquet(3).expect("create test parquet");
        let backend = Arc::new(MockFsBackend::new());
        backend.insert_file("/cached.parquet", parquet);

        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let (_schema, mut stream) = open_row_stream("fs9:///cached.parquet")
                .await
                .expect("open row stream via cached backend");

            let mut rows = Vec::new();
            while let Some(row) = stream.next().await {
                rows.push(row.expect("row conversion should succeed"));
            }

            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[0],
                vec![Value::Int32(0), Value::Text("row_0".to_string())]
            );
        })
        .await;
    }

    #[test]
    fn test_is_fs9_url() {
        assert!(is_fs9_url("fs9:///tmp/file.parquet"));
        assert!(is_fs9_url("fs9://relative/path"));
        assert!(is_fs9_url("FS9:///UPPER/CASE"));
        assert!(!is_fs9_url("http://example.com/file.parquet"));
        assert!(!is_fs9_url("https://example.com/file.parquet"));
        assert!(!is_fs9_url("/local/file.parquet"));
        assert!(!is_fs9_url(""));
    }

    #[test]
    fn test_strip_fs9_scheme() {
        assert_eq!(
            strip_fs9_scheme("fs9:///tmp/file.parquet"),
            "/tmp/file.parquet"
        );
        assert_eq!(strip_fs9_scheme("fs9://relative/path"), "relative/path");
        assert_eq!(strip_fs9_scheme("FS9:///UPPER"), "/UPPER");
        assert_eq!(strip_fs9_scheme("http://example.com"), "http://example.com");
    }
}
