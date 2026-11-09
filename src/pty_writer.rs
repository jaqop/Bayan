//! FIFO input delivery; pipe backpressure must never stop the window thread.
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

pub const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_MESSAGES: usize = 256;

struct PendingWrite {
    bytes: Vec<u8>,
    pending_bytes: Arc<AtomicUsize>,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.pending_bytes
            .fetch_sub(self.bytes.len(), Ordering::Relaxed);
    }
}

pub struct PtyWriter {
    sender: mpsc::SyncSender<PendingWrite>,
    pending_bytes: Arc<AtomicUsize>,
}

impl PtyWriter {
    pub fn spawn(
        mut writer: impl Write + Send + 'static,
        before_write: impl Fn(&[u8]) + Send + 'static,
        on_error: impl Fn(io::Error) + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<PendingWrite>(MAX_PENDING_MESSAGES);
        std::thread::Builder::new()
            .name("bayan-pty-input".into())
            .spawn(move || {
                for packet in receiver {
                    before_write(&packet.bytes);
                    if let Err(error) = writer
                        .write_all(&packet.bytes)
                        .and_then(|()| writer.flush())
                    {
                        on_error(error);
                        break;
                    }
                }
            })?;
        Ok(Self {
            sender,
            pending_bytes: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn try_write(&self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        // Count the in-flight write too: a stuck pipe cannot evade the bound.
        // A plain CAS loop: stable deprecates `fetch_update` for `try_update`,
        // which the 1.89 MSRV does not have.
        let mut current = self.pending_bytes.load(Ordering::Relaxed);
        loop {
            let total = current
                .checked_add(bytes.len())
                .filter(|&total| total <= MAX_PENDING_BYTES)
                .ok_or_else(queue_full)?;
            match self.pending_bytes.compare_exchange_weak(
                current,
                total,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        let packet = PendingWrite {
            bytes: bytes.to_vec(),
            pending_bytes: self.pending_bytes.clone(),
        };
        match self.sender.try_send(packet) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err(queue_full()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the shell input pipe is closed",
            )),
        }
    }
}

fn queue_full() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "Shell input queue is full; this input was not sent. Wait for the shell before retrying.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    struct BlockedPipe {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Write for BlockedPipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_pipe_does_not_block_enqueue_or_drop() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            let input = PtyWriter::spawn(
                BlockedPipe {
                    entered: entered_tx,
                    release: release_rx,
                },
                |_| {},
                |_| {},
            )
            .unwrap();
            input.try_write(b"paste").unwrap();
            drop(input);
            done_tx.send(()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let responsive = done_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        release_tx.send(()).unwrap();
        caller.join().unwrap();
        assert!(responsive, "the UI caller waited for the blocked pipe");
    }

    #[test]
    fn oversized_input_is_rejected_as_a_whole() {
        let input = PtyWriter::spawn(io::sink(), |_| {}, |_| {}).unwrap();
        assert_eq!(
            input
                .try_write(&vec![b'x'; MAX_PENDING_BYTES + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn byte_budget_counts_the_blocked_write_and_rejects_the_whole_next_message() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let input = PtyWriter::spawn(
            BlockedPipe {
                entered: entered_tx,
                release: release_rx,
            },
            |_| {},
            |_| {},
        )
        .unwrap();
        let half = vec![b'x'; MAX_PENDING_BYTES / 2];
        input.try_write(&half).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let queued = input.try_write(&half);
        let rejected = input.try_write(b"\x1b[200~whole paste\x1b[201~");
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(queued.is_ok());
        assert_eq!(rejected.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn message_budget_bounds_small_keystrokes_and_recovers_after_drain() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let input = PtyWriter::spawn(
            BlockedPipe {
                entered: entered_tx,
                release: release_rx,
            },
            |_| {},
            |_| {},
        )
        .unwrap();
        input.try_write(b"first").unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        for _ in 0..MAX_PENDING_MESSAGES {
            input.try_write(b"x").unwrap();
        }
        let rejected = input.try_write(b"y");
        for _ in 0..=MAX_PENDING_MESSAGES {
            release_tx.send(()).unwrap();
        }
        for _ in 0..MAX_PENDING_MESSAGES {
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(rejected.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        // Every accepted write has now left the queue; a rejected packet
        // must have released its reservation too.
        release_tx.send(()).unwrap();
        input.try_write(b"retry").unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn worker_write_failures_are_reported() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed by child"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (errors_tx, errors_rx) = mpsc::channel();
        let input = PtyWriter::spawn(
            BrokenPipe,
            |_| {},
            move |e| {
                errors_tx.send(e.kind()).unwrap();
            },
        )
        .unwrap();
        input.try_write(b"x").unwrap();
        assert_eq!(
            errors_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn accepted_input_keeps_fifo_order_and_capture_precedes_write() {
        struct RecordingPipe(Arc<Mutex<Vec<u8>>>);
        impl Write for RecordingPipe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let capture = output.clone();
        let (seen_tx, seen_rx) = mpsc::channel();
        let input = PtyWriter::spawn(
            RecordingPipe(output.clone()),
            move |bytes| {
                if bytes == b"\r" {
                    assert_eq!(*capture.lock().unwrap(), b"echo hello");
                    seen_tx.send(()).unwrap();
                }
            },
            |_| {},
        )
        .unwrap();
        input.try_write(b"echo ").unwrap();
        input.try_write(b"hello").unwrap();
        input.try_write(b"\r").unwrap();
        seen_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(input);
    }
}
