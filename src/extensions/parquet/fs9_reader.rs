//! Fs9 backend parquet reader.
//!
//! Provides in-memory parquet file reading from fs9 backend.

use bytes::Bytes;
use futures::future::BoxFuture;
use futures::FutureExt;
use parquet::arrow::async_reader::AsyncFileReader;
use parquet::errors::{ParquetError, Result as ParquetResult};
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use std::ops::Range;
use std::sync::Arc;

pub(crate) const MAX_FS9_PARQUET_FILE_BYTES: usize = 100 * 1024 * 1024;

pub(crate) struct Fs9ParquetReader {
    data: Bytes,
}

impl Fs9ParquetReader {
    pub(crate) fn new(data: Bytes) -> Self {
        Self { data }
    }
}

impl AsyncFileReader for Fs9ParquetReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, ParquetResult<Bytes>> {
        let data = self.data.clone();
        async move {
            let start = usize::try_from(range.start).map_err(|e| {
                ParquetError::General(format!("range start conversion failed: {e}"))
            })?;
            let end = usize::try_from(range.end)
                .map_err(|e| ParquetError::General(format!("range end conversion failed: {e}")))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_array::{ArrayRef, Int32Array, StringArray};
    use arrow_schema::{DataType as ArrowDataType, Field, Schema as ArrowSchema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;

    use crate::extensions::parquet::reader::infer_schema_from_reader;
    use futures::StreamExt;

    fn make_test_parquet(num_rows: usize) -> anyhow::Result<Bytes> {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("id", ArrowDataType::Int32, false),
            Field::new("name", ArrowDataType::Utf8, true),
        ]));

        let ids: Vec<i32> = (0..num_rows).map(|i| i as i32).collect();
        let names: Vec<Option<String>> = (0..num_rows).map(|i| Some(format!("row_{i}"))).collect();

        let batch = arrow_array::RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(ids)) as ArrayRef,
                Arc::new(StringArray::from(names)) as ArrayRef,
            ],
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let props = WriterProperties::builder()
            .set_max_row_group_size(5)
            .build();
        let mut out = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut out, schema, Some(props))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            writer.write(&batch).map_err(|e| anyhow::anyhow!("{e}"))?;
            writer.close().map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        Ok(Bytes::from(out))
    }

    #[tokio::test]
    async fn test_schema_inference() {
        let data = make_test_parquet(10).expect("create test parquet");
        let reader = Fs9ParquetReader::new(data);

        let schema = infer_schema_from_reader(reader)
            .await
            .expect("infer schema should succeed");

        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[1].name, "name");
    }

    #[tokio::test]
    async fn test_row_streaming() {
        let data = make_test_parquet(15).expect("create test parquet");
        let reader = Fs9ParquetReader::new(data);

        let (_schema, mut stream) =
            crate::extensions::parquet::reader::open_row_stream_from_reader(reader, 4)
                .await
                .expect("open row stream should succeed");

        let mut row_count = 0;
        while let Some(_row) = stream.next().await {
            row_count += 1;
        }

        assert_eq!(row_count, 15);
    }

    #[tokio::test]
    async fn test_invalid_bytes() {
        let invalid_data = Bytes::from(vec![0xFF, 0xFF, 0xFF]);
        let reader = Fs9ParquetReader::new(invalid_data);

        let result = infer_schema_from_reader(reader).await;
        assert!(
            result.is_err(),
            "should return error for invalid parquet bytes"
        );
    }
}
