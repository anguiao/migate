use futures_lite::io::AsyncWrite;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

/// One encoded MQTT frame and its write progress. The frame owns partial
/// writes, offset accounting and flush completion; request identity,
/// eligibility, deadlines and cancellation stay with the driver.
pub(in crate::xiaomi) struct SendFrame {
    bytes: Vec<u8>,
    offset: usize,
}

/// Progress of one `poll_io` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::xiaomi) enum SendStatus {
    /// No bytes reached the socket and no flush completed.
    Pending,
    /// The socket accepted `accepted` bytes. `complete` reports whether the
    /// whole frame is now written; the driver records this progress first and
    /// may then ask for the flush in the same poll, so an accepted write can
    /// never be lost behind a flush error.
    Wrote { accepted: usize, complete: bool },
    /// The frame is fully written and flushed; the driver may retire it.
    Flushed,
}

impl SendFrame {
    pub(in crate::xiaomi) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(in crate::xiaomi) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(in crate::xiaomi) fn offset(&self) -> usize {
        self.offset
    }

    /// True once any non-empty prefix reached the socket. Only such a frame may
    /// be treated as possibly sent.
    pub(in crate::xiaomi) fn started(&self) -> bool {
        self.offset > 0
    }

    /// Drive the front frame, reporting accepted bytes immediately so the
    /// caller can record send progress before any flush.
    pub(in crate::xiaomi) fn poll_io<S>(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut S,
    ) -> io::Result<SendStatus>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        if self.offset < self.bytes.len() {
            return match Pin::new(&mut *stream).poll_write(cx, &self.bytes[self.offset..]) {
                Poll::Ready(Ok(0)) => Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "MQTT transport wrote zero bytes",
                )),
                Poll::Ready(Ok(count)) => {
                    self.offset += count;
                    Ok(SendStatus::Wrote {
                        accepted: count,
                        complete: self.offset == self.bytes.len(),
                    })
                }
                Poll::Ready(Err(error)) => Err(error),
                Poll::Pending => Ok(SendStatus::Pending),
            };
        }
        Ok(if self.poll_flush(cx, stream)? {
            SendStatus::Flushed
        } else {
            SendStatus::Pending
        })
    }

    /// Flush a frame whose bytes are all written. Returns true when the
    /// transport accepted the flush; it never writes more bytes.
    pub(in crate::xiaomi) fn poll_flush<S>(
        &mut self,
        cx: &mut Context<'_>,
        stream: &mut S,
    ) -> io::Result<bool>
    where
        S: AsyncWrite + Unpin + ?Sized,
    {
        debug_assert_eq!(
            self.offset,
            self.bytes.len(),
            "flush before the frame is written"
        );
        match Pin::new(&mut *stream).poll_flush(cx) {
            Poll::Ready(Ok(())) => Ok(true),
            Poll::Ready(Err(error)) => Err(error),
            Poll::Pending => Ok(false),
        }
    }
}
