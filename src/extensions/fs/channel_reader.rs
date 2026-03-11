use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};
use tokio::sync::mpsc;

pub(crate) struct ChunkReceiverReader {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    current_chunk: Vec<u8>,
    current_offset: usize,
    finished: bool,
}

impl ChunkReceiverReader {
    pub(crate) fn new(receiver: mpsc::Receiver<io::Result<Vec<u8>>>) -> Self {
        Self {
            receiver,
            current_chunk: Vec::new(),
            current_offset: 0,
            finished: false,
        }
    }

    fn has_buffered_data(&self) -> bool {
        self.current_offset < self.current_chunk.len()
    }

    fn buffered_slice(&self) -> &[u8] {
        &self.current_chunk[self.current_offset..]
    }

    fn poll_load_chunk(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            if this.has_buffered_data() || this.finished {
                return Poll::Ready(Ok(()));
            }

            match Pin::new(&mut this.receiver).poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => {
                    this.current_chunk = chunk;
                    this.current_offset = 0;
                }
                Poll::Ready(Some(Err(err))) => {
                    this.finished = true;
                    this.current_chunk.clear();
                    this.current_offset = 0;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    this.current_chunk.clear();
                    this.current_offset = 0;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl AsyncRead for ChunkReceiverReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.as_mut().poll_load_chunk(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {
                let available = self.buffered_slice();
                if available.is_empty() {
                    return Poll::Ready(Ok(()));
                }

                let to_copy = available.len().min(buf.remaining());
                buf.put_slice(&available[..to_copy]);
                self.current_offset += to_copy;
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncBufRead for ChunkReceiverReader {
    fn poll_fill_buf(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        match self.as_mut().poll_load_chunk(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {
                let this = self.get_mut();
                Poll::Ready(Ok(&this.current_chunk[this.current_offset..]))
            }
        }
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        self.current_offset = self
            .current_offset
            .saturating_add(amt)
            .min(self.current_chunk.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufReader};

    #[tokio::test]
    async fn test_chunk_receiver_reader_reads_all_chunks_in_order() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(Ok(b"hello ".to_vec())).await.unwrap();
        tx.send(Ok(b"world".to_vec())).await.unwrap();
        drop(tx);

        let mut reader = BufReader::new(ChunkReceiverReader::new(rx));
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn test_chunk_receiver_reader_propagates_stream_error() {
        let (tx, rx) = mpsc::channel(2);
        tx.send(Err(io::Error::other("stream failed")))
            .await
            .unwrap();
        drop(tx);

        let mut reader = ChunkReceiverReader::new(rx);
        let mut buf = [0u8; 16];
        let err = reader.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.to_string(), "stream failed");
    }
}
