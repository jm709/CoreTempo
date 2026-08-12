//! Per-agent ~256 KiB replay ring with monotonic byte cursors, plus the
//! 8 ms / 32 KB read coalescer (Task 10).

use std::collections::VecDeque;

use crate::pty::Cursor;

/// 256 KiB per agent (frozen constant).
pub(crate) const RING_CAPACITY: usize = 256 * 1024;

/// Fixed-capacity byte ring. `written` counts every byte ever pushed, so
/// cursors are monotonic across ring wrap AND across agent restarts (the ring
/// outlives sessions).
pub(crate) struct ReplayRing {
    buf: VecDeque<u8>,
    capacity: usize,
    written: u64,
}

impl ReplayRing {
    pub(crate) fn new(capacity: usize) -> ReplayRing {
        ReplayRing {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            written: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.capacity {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.capacity..]);
        } else {
            let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.capacity);
            self.buf.drain(..overflow);
            self.buf.extend(bytes);
        }
        self.written += bytes.len() as u64;
    }

    /// Cursor of the oldest byte still in the ring.
    pub(crate) fn start(&self) -> u64 {
        self.written - self.buf.len() as u64
    }

    /// Cursor one past the newest byte ever written.
    pub(crate) fn end(&self) -> Cursor {
        Cursor(self.written)
    }

    /// (cursor after last byte, bytes from `max(since, start)`).
    pub(crate) fn read_since(&self, since: Option<Cursor>) -> (Cursor, Vec<u8>) {
        let from = since.map_or(self.start(), |c| c.0.clamp(self.start(), self.written));
        let skip = usize::try_from(from - self.start()).unwrap_or(usize::MAX);
        (self.end(), self.buf.iter().skip(skip).copied().collect())
    }
}

/// Flush whichever comes first: 8 ms elapsed since the batch's first byte, or
/// 32 KB accumulated. Collapses TUI redraw storms (frozen constants).
pub(crate) const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);
pub(crate) const FLUSH_BYTES: usize = 32 * 1024;

/// Coalesces raw PTY reads into flushes. Returns when `rx` closes, after
/// flushing any pending remainder.
pub(crate) async fn coalesce<F>(mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>, mut flush: F)
where
    F: FnMut(Vec<u8>),
{
    let mut pending: Vec<u8> = Vec::new();
    loop {
        if pending.is_empty() {
            match rx.recv().await {
                Some(bytes) => pending.extend(bytes),
                None => return,
            }
        }
        let deadline = tokio::time::Instant::now() + FLUSH_INTERVAL;
        while pending.len() < FLUSH_BYTES {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                more = rx.recv() => {
                    let Some(bytes) = more else {
                        flush(std::mem::take(&mut pending));
                        return;
                    };
                    pending.extend(bytes);
                }
            }
        }
        flush(std::mem::take(&mut pending));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::pty::Cursor;
    use crate::pty::ring::{FLUSH_BYTES, FLUSH_INTERVAL, ReplayRing, coalesce};

    fn harness() -> (mpsc::Sender<Vec<u8>>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(64);
        let (flush_tx, flush_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(coalesce(raw_rx, move |chunk| {
            let _ = flush_tx.send(chunk);
        }));
        (raw_tx, flush_rx)
    }

    #[tokio::test(start_paused = true)]
    async fn small_writes_flush_after_8ms() {
        let (tx, mut flushed) = harness();
        tx.send(b"ab".to_vec()).await.unwrap();
        tx.send(b"cd".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(flushed.try_recv().is_err(), "must not flush before 8 ms");
        tokio::time::sleep(FLUSH_INTERVAL).await;
        assert_eq!(flushed.recv().await.unwrap(), b"abcd".to_vec());
    }

    #[tokio::test(start_paused = true)]
    async fn large_write_flushes_immediately_without_time_advance() {
        let (tx, mut flushed) = harness();
        tx.send(vec![b'x'; FLUSH_BYTES + 5]).await.unwrap();
        let chunk = flushed.recv().await.unwrap();
        assert_eq!(chunk.len(), FLUSH_BYTES + 5);
    }

    #[tokio::test(start_paused = true)]
    async fn sender_close_flushes_remainder() {
        let (tx, mut flushed) = harness();
        tx.send(b"tail".to_vec()).await.unwrap();
        drop(tx);
        assert_eq!(flushed.recv().await.unwrap(), b"tail".to_vec());
        assert!(
            flushed.recv().await.is_none(),
            "coalescer exits after close"
        );
    }

    #[test]
    fn cursors_advance_monotonically() {
        let mut ring = ReplayRing::new(8);
        assert_eq!(ring.end(), Cursor(0));
        ring.push(b"abc");
        assert_eq!(ring.end(), Cursor(3));
        assert_eq!(ring.start(), 0);
        ring.push(b"defgh");
        assert_eq!(ring.end(), Cursor(8));
        assert_eq!(ring.start(), 0);
    }

    #[test]
    fn overflow_drops_oldest_but_keeps_cursor() {
        let mut ring = ReplayRing::new(8);
        ring.push(b"abcdefgh");
        ring.push(b"ij");
        assert_eq!(ring.end(), Cursor(10));
        assert_eq!(ring.start(), 2, "oldest two bytes aged out");
        let (end, bytes) = ring.read_since(None);
        assert_eq!(end, Cursor(10));
        assert_eq!(bytes, b"cdefghij".to_vec());
    }

    #[test]
    fn push_larger_than_capacity_keeps_tail() {
        let mut ring = ReplayRing::new(4);
        ring.push(b"0123456789");
        assert_eq!(ring.end(), Cursor(10));
        assert_eq!(ring.start(), 6);
        assert_eq!(ring.read_since(None).1, b"6789".to_vec());
    }

    #[test]
    fn read_since_clamps_to_ring_window() {
        let mut ring = ReplayRing::new(8);
        ring.push(b"abcdefghij"); // start = 2, end = 10
        // in-window cursor
        assert_eq!(ring.read_since(Some(Cursor(6))).1, b"ghij".to_vec());
        // aged-out cursor clamps to start (consumer detects via start() > since)
        assert_eq!(ring.read_since(Some(Cursor(0))).1, b"cdefghij".to_vec());
        // future cursor clamps to end: empty tail
        assert_eq!(ring.read_since(Some(Cursor(99))).1, Vec::<u8>::new());
    }
}
