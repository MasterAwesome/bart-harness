use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt as _};

pub(super) struct Reader<R> {
    inner: R,
    pub(super) frame: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> Reader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            frame: Vec::new(),
        }
    }

    pub(super) async fn read(&mut self, limit: usize) -> io::Result<usize> {
        loop {
            if self.frame.len() > limit {
                return Err(overflow(limit));
            }
            if self.frame.last() == Some(&b'\n') {
                return Ok(self.frame.len());
            }
            if self.frame.len() == limit {
                return Err(overflow(limit));
            }
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if self.frame.is_empty() {
                    Ok(0)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete interpreter frame",
                    ))
                };
            }
            let length = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |at| at + 1);
            let take = length.min(limit - self.frame.len());
            self.frame.extend_from_slice(&available[..take]);
            self.inner.consume(take);
        }
    }
}

fn overflow(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("interpreter frame exceeds {limit} bytes (including LF)"),
    )
}
