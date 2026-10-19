use super::*;
use futures_lite::io::AsyncWrite;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll, Waker},
};

struct ControlledWrite {
    budget: usize,
    flush_pending: bool,
    fail_flush: bool,
    written: Vec<u8>,
}

impl AsyncWrite for ControlledWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let count = bytes.len().min(self.budget);
        if count == 0 {
            return Poll::Pending;
        }
        self.budget -= count;
        self.written.extend_from_slice(&bytes[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.fail_flush {
            return Poll::Ready(Err(io::Error::other("flush failed")));
        }
        if self.flush_pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn writer(budget: usize) -> ControlledWrite {
    ControlledWrite {
        budget,
        flush_pending: false,
        fail_flush: false,
        written: Vec::new(),
    }
}

fn step(frame: &mut SendFrame, stream: &mut ControlledWrite) -> SendStatus {
    let mut cx = Context::from_waker(Waker::noop());
    frame.poll_io(&mut cx, stream).expect("send status")
}

#[test]
fn partial_writes_accumulate_before_flush() {
    let mut frame = SendFrame::new(b"hello".to_vec());
    let mut stream = writer(2);
    assert!(!frame.started());
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Wrote {
            accepted: 2,
            complete: false,
        }
    );
    assert!(frame.started());
    assert_eq!(frame.offset(), 2);
    assert_eq!(frame.bytes(), b"hello");
    stream.budget = 100;
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Wrote {
            accepted: 3,
            complete: true,
        }
    );
    assert_eq!(stream.written, b"hello");
    assert_eq!(step(&mut frame, &mut stream), SendStatus::Flushed);
}

#[test]
fn a_completed_write_is_reported_before_flush_is_attempted() {
    let mut frame = SendFrame::new(b"xyz".to_vec());
    let mut stream = writer(10);
    stream.flush_pending = true;
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Wrote {
            accepted: 3,
            complete: true,
        },
        "the full write is reported in the poll that accepted it"
    );
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Pending,
        "flush is a separate step once the frame is fully written"
    );
    stream.flush_pending = false;
    assert_eq!(step(&mut frame, &mut stream), SendStatus::Flushed);
}

#[test]
fn flush_follows_an_accepted_write_without_another_write() {
    let mut frame = SendFrame::new(b"ok".to_vec());
    let mut stream = writer(10);
    stream.flush_pending = true;
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Wrote {
            accepted: 2,
            complete: true,
        }
    );
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(
        frame.poll_io(&mut cx, &mut stream).unwrap(),
        SendStatus::Pending
    );
    stream.flush_pending = false;
    assert_eq!(
        frame.poll_io(&mut cx, &mut stream).unwrap(),
        SendStatus::Flushed
    );
    assert_eq!(stream.written, b"ok", "flush never writes bytes");
}

#[test]
fn a_flush_error_after_an_accepted_write_cannot_erase_the_progress() {
    let mut frame = SendFrame::new(b"abcd".to_vec());
    let mut stream = writer(10);
    assert_eq!(
        step(&mut frame, &mut stream),
        SendStatus::Wrote {
            accepted: 4,
            complete: true,
        }
    );
    stream.fail_flush = true;
    let mut cx = Context::from_waker(Waker::noop());
    assert!(frame.poll_io(&mut cx, &mut stream).is_err());
    assert!(frame.started(), "the accepted prefix is still recorded");
    assert_eq!(frame.offset(), 4);
}

#[test]
fn a_level_triggered_write_makes_no_progress_without_a_budget() {
    let mut frame = SendFrame::new(b"blocked".to_vec());
    let mut stream = writer(0);
    assert_eq!(step(&mut frame, &mut stream), SendStatus::Pending);
    assert!(!frame.started());
}

#[test]
fn a_zero_byte_write_is_an_io_error() {
    struct ZeroWrite;
    impl AsyncWrite for ZeroWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let mut frame = SendFrame::new(b"data".to_vec());
    let mut cx = Context::from_waker(Waker::noop());
    let error = frame
        .poll_io(&mut cx, &mut ZeroWrite)
        .expect_err("zero write must fail");
    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
    assert!(!frame.started());
}

#[test]
fn an_empty_frame_completes_through_flush() {
    let mut frame = SendFrame::new(Vec::new());
    let mut stream = writer(0);
    assert_eq!(step(&mut frame, &mut stream), SendStatus::Flushed);
}
