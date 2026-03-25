use super::*;

pub(crate) async fn start_file_stream(
    tenant: &str,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let backend = backend::acquire_statement_backend(tenant).await?;
    start_file_stream_for_backend(backend, path, format, delimiter, header).await
}

async fn start_file_stream_for_backend(
    backend: Arc<dyn backend::FsBackend>,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    let info = backend.stat(path).await?;
    if info.is_dir {
        return Ok(None);
    }

    let fmt = decoders::detect_format(path, format);

    #[cfg(feature = "parquet")]
    if fmt == "parquet" {
        let data = backend
            .read_file(
                path,
                crate::extensions::parquet::fs9_reader::MAX_FS9_PARQUET_FILE_BYTES,
            )
            .await?;
        let reader =
            crate::extensions::parquet::fs9_reader::Fs9ParquetReader::new(bytes::Bytes::from(data));
        let (parquet_schema, row_stream) =
            crate::extensions::parquet::reader::open_row_stream_from_reader(reader, 8192)
                .await
                .map_err(|e| anyhow::anyhow!("fs9: parquet stream error: {e}"))?;
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            futures::pin_mut!(row_stream);
            while let Some(values) = futures::StreamExt::next(&mut row_stream).await {
                match values {
                    Ok(vals) => {
                        if tx.send(crate::model::Row::new(vals)).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("fs9: parquet streaming error: {}", err);
                        break;
                    }
                }
            }
        });
        return Ok(Some((parquet_schema, rx)));
    }

    #[cfg(not(feature = "parquet"))]
    if fmt == "parquet" {
        return Err(anyhow::anyhow!(
            "fs9: parquet format requires the parquet extension (compile with --features parquet)"
        ));
    }

    let reader = backend.read_file_stream(path, MAX_BYTES_PER_FILE).await?;

    let (schema, rx) = match fmt {
        "csv" | "tsv" => {
            let delim = if fmt == "tsv" && delimiter.is_none() {
                Some('\t')
            } else {
                delimiter
            };
            let has_headers = header.unwrap_or(true);
            let mut decoder =
                streaming::StreamingCsvDecoder::new(reader, path.to_string(), delim, has_headers)
                    .await
                    .map_err(|(e, _)| e)?;
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
        "jsonl" | "ndjson" => {
            let mut decoder = streaming::StreamingJsonlDecoder::new(reader, path.to_string());
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
        _ => {
            let mut decoder = streaming::StreamingTextDecoder::new(reader, path.to_string());
            let schema = decoder.schema().clone();
            let (tx, rx) = mpsc::channel(256);
            let stream_path = path.to_string();
            tokio::spawn(async move {
                loop {
                    match decoder.next_row().await {
                        Ok(Some(row)) => {
                            if tx.send(row).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!("fs9: streaming decode error for {}: {}", stream_path, err);
                            break;
                        }
                    }
                }
            });
            (schema, rx)
        }
    };

    Ok(Some((schema, rx)))
}

#[cfg(test)]
pub(crate) async fn start_file_stream_for_test_backend(
    backend: Box<dyn backend::FsBackend>,
    path: &str,
    format: Option<&str>,
    delimiter: Option<char>,
    header: Option<bool>,
) -> Result<Option<(TableSchema, mpsc::Receiver<Row>)>> {
    start_file_stream_for_backend(Arc::from(backend), path, format, delimiter, header).await
}
