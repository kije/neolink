//! Named pipe plumbing.
//!
//! A FIFO is not quite a file and not quite a socket, and the difference
//! matters twice here.
//!
//! Opening one for writing blocks until a reader opens the other end.
//! That is a useful signal — it is exactly when the camera should be
//! connected — but waiting for it on a thread would park one per idle
//! camera, and a parked thread cannot be cancelled. Opening non-blocking
//! instead turns "no reader yet" into an `ENXIO` we can poll for, so an
//! unwatched camera costs a syscall every [`POLL`] and nothing else.
//!
//! Writing to one can then block in its own right, when the reader is
//! slower than the camera. Registering the descriptor with the reactor
//! keeps that off the thread pool as well, so a stalled reader parks
//! nothing at all — its chunks simply queue, and the fanout discards them
//! once it has fallen far enough behind.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use tokio::io::unix::AsyncFd;
use tokio::time::{sleep, Duration};

use crate::stream::fanout::Consumer;

/// How often to look for a reader having turned up.
///
/// Only the delay before an already-waiting reader is served, so it can
/// afford to be lazy; a quarter of a second is far inside the time the
/// camera takes to produce its first keyframe anyway.
const POLL: Duration = Duration::from_millis(250);

/// Create the FIFO, unless it is already there.
pub(crate) fn create(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_fifo() {
                bail!("{path:?} already exists and is not a FIFO");
            }
            // Reuse it. Recreating would break a reader that is already
            // attached to the old one.
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("Could not stat {path:?}")),
    }

    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{path:?} cannot be used as a path"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, which is all `mkfifo` asks of us.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) } != 0 {
        let error = std::io::Error::last_os_error();
        // Something else got there between the stat and here.
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error).with_context(|| format!("Could not create the FIFO {path:?}"));
        }
    }
    Ok(())
}

/// Open the FIFO for writing, returning once a reader is attached.
///
/// The returned descriptor is non-blocking and registered with the
/// runtime, so writes to it never occupy a thread.
pub(crate) async fn open_when_read(path: &Path) -> Result<AsyncFd<File>> {
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                return AsyncFd::new(file)
                    .with_context(|| format!("Could not watch {path:?} for writability"))
            }
            // ENXIO on a non-blocking write-open means precisely "no
            // reader yet", which is the ordinary idle state.
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => sleep(POLL).await,
            Err(e) => return Err(e).with_context(|| format!("Could not open {path:?}")),
        }
    }
}

/// Write everything a consumer is given to the pipe.
pub(crate) async fn write_consumer(consumer: &Consumer, pipe: AsyncFd<File>) -> Result<()> {
    while let Some(chunk) = consumer.next().await {
        write_all(&pipe, &chunk.bytes).await?;
    }
    Ok(())
}

/// `write_all`, but on a descriptor the runtime is polling for us.
///
/// A pipe accepts a bounded amount before it fills, so a partial write is
/// normal rather than exceptional and the loop is doing real work.
async fn write_all(pipe: &AsyncFd<File>, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        let mut guard = pipe.writable().await?;
        match guard.try_io(|inner| {
            let mut file: &File = inner.get_ref();
            file.write(buf)
        }) {
            Ok(Ok(0)) => bail!("the pipe accepted no bytes"),
            Ok(Ok(written)) => buf = &buf[written..],
            Ok(Err(e)) => return Err(e.into()),
            // Readiness was a false alarm; ask again.
            Err(_would_block) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::fanout::{Chunk, Fanout};
    use std::io::Read;

    /// A scratch directory of our own, removed when the test ends.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "neolink-fifo-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("could not make a scratch directory");
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn create_makes_a_fifo_and_is_happy_to_find_one() {
        let scratch = Scratch::new("create");
        let path = scratch.path("cam.ts");

        create(&path).expect("should create");
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo());

        // Running twice must not disturb a reader already attached to the
        // one that is there.
        create(&path).expect("should accept an existing FIFO");
    }

    #[test]
    fn create_refuses_to_clobber_something_that_is_not_a_fifo() {
        let scratch = Scratch::new("clobber");
        let path = scratch.path("cam.ts");
        std::fs::write(&path, b"a real file").unwrap();

        assert!(
            create(&path).is_err(),
            "a plain file at the endpoint path should be an error, not a deletion"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"a real file");
    }

    #[tokio::test]
    async fn opening_waits_for_a_reader_and_then_delivers() {
        let scratch = Scratch::new("open");
        let path = scratch.path("cam.ts");
        create(&path).unwrap();

        // Nothing is reading, so this must not resolve.
        let opening = tokio::spawn({
            let path = path.clone();
            async move { open_when_read(&path).await }
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !opening.is_finished(),
            "an unread FIFO should leave the camera alone"
        );

        // Opening the read end is the signal to start.
        let reader = tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(&path).unwrap();
            let mut got = Vec::new();
            file.read_to_end(&mut got).unwrap();
            got
        });

        let pipe = tokio::time::timeout(Duration::from_secs(5), opening)
            .await
            .expect("a reader attaching should complete the open")
            .unwrap()
            .unwrap();

        let fanout = Fanout::new(None);
        let consumer = fanout.attach();
        fanout.push(&Chunk::new(b"hello ".to_vec(), true, 0));
        fanout.push(&Chunk::new(b"pipe".to_vec(), false, 1_000));
        fanout.close();

        write_consumer(&consumer, pipe).await.unwrap();
        drop(fanout);

        let got = tokio::time::timeout(Duration::from_secs(5), reader)
            .await
            .expect("the reader should see EOF once we close")
            .unwrap();
        assert_eq!(got, b"hello pipe");
    }

    #[tokio::test]
    async fn writing_to_a_departed_reader_reports_a_broken_pipe() {
        let scratch = Scratch::new("epipe");
        let path = scratch.path("cam.ts");
        create(&path).unwrap();

        let opening = tokio::spawn({
            let path = path.clone();
            async move { open_when_read(&path).await }
        });

        // Attach and immediately leave, which is what a reader crashing
        // looks like from this end. Opening the read end of a FIFO blocks
        // until a writer arrives, so it cannot be done on the runtime
        // thread — `#[tokio::test]` is single threaded, and blocking it
        // would deadlock against the open above.
        let reader = tokio::task::spawn_blocking({
            let path = path.clone();
            move || std::fs::File::open(&path).unwrap()
        })
        .await
        .unwrap();
        let pipe = tokio::time::timeout(Duration::from_secs(5), opening)
            .await
            .expect("open should complete")
            .unwrap()
            .unwrap();
        drop(reader);

        let fanout = Fanout::new(None);
        let consumer = fanout.attach();
        // Enough to outrun the kernel's pipe buffer, so the write cannot
        // quietly succeed into a buffer nobody will ever read.
        for i in 0..64 {
            fanout.push(&Chunk::new(vec![0u8; 16 * 1024], i == 0, i * 1_000));
        }
        fanout.close();

        let result = tokio::time::timeout(Duration::from_secs(10), write_consumer(&consumer, pipe))
            .await
            .expect("writing to a departed reader should fail rather than hang");
        let error = result.expect_err("writing to a departed reader should fail");
        assert!(
            crate::stream::is_disconnect(&error),
            "the failure should be recognised as a disconnect, got {:?}",
            error
        );
    }
}
