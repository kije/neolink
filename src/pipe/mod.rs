//!
//! # Neolink Pipe
//!
//! Serves cameras over FIFOs and unix sockets in a directory, so that a
//! reader in a different container can get at them.
//!
//! # Usage
//!
//! ```bash
//! neolink pipe --config=/etc/neolink.toml --dir=/run/neolink
//! ```
//!
//! [`crate::stream`] covers the case where the reader can run neolink
//! itself, via go2rtc's `exec:`. That does not work when neolink is on the
//! host and go2rtc is in Docker: `exec:` runs the command *inside* the
//! container, which would need neolink, its config and the camera's
//! network there too. A path in a bind-mounted directory crosses that
//! boundary without any of it:
//!
//! ```yaml
//! # docker-compose.yml
//! volumes:
//!   - /run/neolink:/pipes
//!
//! # go2rtc
//! streams:
//!   front: exec:cat /pipes/Front.ts
//! ```
//!
//! # Nothing runs unless somebody is watching
//!
//! A camera is connected only while a reader is attached to its endpoint,
//! and released as soon as the last one goes. That is the same on-demand
//! behaviour `exec:` gets by starting and stopping the process, without
//! needing the process to be startable.
//!
//! # A stalled reader never becomes a backlog
//!
//! The point of `--max-backlog`. If a reader crashes, hangs, or is simply
//! slower than the camera, queuing what it missed is the wrong answer: it
//! would come back and play the gap out, permanently that far behind. Past
//! the limit its queue is discarded and it resumes at the next keyframe,
//! so a recovered reader sees what the camera is doing now. Each reader is
//! measured on its own, so a slow one cannot hold up a fast one, and
//! neither can hold up the camera.

use anyhow::{anyhow, Context, Result};
use log::*;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};

mod cmdline;
#[cfg(unix)]
mod fifo;

use crate::common::NeoReactor;
use crate::stream::fanout::Fanout;
use crate::stream::pump::{learn, Packer};
use crate::stream::{is_disconnect, report_overruns, write_consumer};
pub(crate) use cmdline::Opt;

/// How long to wait before reconnecting a camera that failed.
///
/// Only reached when a reader is attached and the camera itself is not
/// working, so it wants to be short enough to recover promptly and long
/// enough not to hammer a camera that is down.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Entry point for the pipe subcommand
///
/// Opt is the command line options
pub(crate) async fn main(opt: Opt, reactor: NeoReactor) -> Result<()> {
    let cameras = wanted_cameras(&opt, &reactor).await?;
    if cameras.is_empty() {
        return Err(anyhow!("No cameras to serve"));
    }

    tokio::fs::create_dir_all(&opt.dir)
        .await
        .with_context(|| format!("Could not create {:?}", opt.dir))?;

    info!(
        "Serving {} camera(s) from {:?}: {}",
        cameras.len(),
        opt.dir,
        cameras.join(", ")
    );

    let opt = Arc::new(opt);
    let mut set = JoinSet::new();
    for name in cameras {
        set.spawn(serve_camera(name, opt.clone(), reactor.clone()));
    }

    // Every camera runs forever, so the first one to return has failed
    // and there is no sense carrying on with a partial set.
    while let Some(joined) = set.join_next().await {
        joined.context("A camera task panicked")??;
    }
    Ok(())
}

/// The cameras named on the command line, or every enabled one.
async fn wanted_cameras(opt: &Opt, reactor: &NeoReactor) -> Result<Vec<String>> {
    let config = reactor.config().await?.borrow().clone();
    let known: Vec<String> = config
        .cameras
        .iter()
        .filter(|camera| camera.enabled)
        .map(|camera| camera.name.clone())
        .collect();

    if opt.camera.is_empty() {
        return Ok(known);
    }
    for name in &opt.camera {
        if !known.iter().any(|k| k == name) {
            return Err(anyhow!(
                "Camera `{name}` is not an enabled camera in the config"
            ));
        }
    }
    Ok(opt.camera.clone())
}

/// Serve one camera: its endpoints, and the source that feeds them.
async fn serve_camera(name: String, opt: Arc<Opt>, reactor: NeoReactor) -> Result<()> {
    let fanout = Arc::new(Fanout::new(opt.backlog()));
    let mut set = JoinSet::new();

    if opt.endpoint.fifo() {
        let path = opt.dir.join(format!("{name}.ts"));
        set.spawn(serve_fifo(path, name.clone(), fanout.clone()));
    }
    if opt.endpoint.socket() {
        let path = opt.dir.join(format!("{name}.sock"));
        set.spawn(serve_socket(path, name.clone(), fanout.clone()));
    }

    set.spawn(run_source(name.clone(), opt, reactor, fanout));

    while let Some(joined) = set.join_next().await {
        joined.with_context(|| format!("{name}: a task panicked"))??;
    }
    Ok(())
}

/// Keep the camera connected for exactly as long as somebody is reading.
async fn run_source(
    name: String,
    opt: Arc<Opt>,
    reactor: NeoReactor,
    fanout: Arc<Fanout>,
) -> Result<()> {
    let kind = opt.stream.into();
    loop {
        fanout.wait_for_consumer().await;
        debug!("{name}::{kind}: a reader attached, connecting the camera");

        if let Err(e) = stream_once(&name, &opt, &reactor, &fanout, kind).await {
            warn!("{name}::{kind}: {e:?}");
            // Do not spin on a camera that is refusing to talk, but do
            // come back: the reader is still there waiting.
            sleep(RETRY_DELAY).await;
        } else {
            debug!("{name}::{kind}: no readers left, releasing the camera");
        }
    }
}

/// One camera session, from connect until the last reader goes.
async fn stream_once(
    name: &str,
    opt: &Opt,
    reactor: &NeoReactor,
    fanout: &Fanout,
    kind: neolink_core::bc_protocol::StreamKind,
) -> Result<()> {
    let camera = reactor.get(name).await?;
    let mut media = camera
        .stream(kind)
        .await
        .with_context(|| format!("Could not start streaming {name}"))?;

    let learned = learn(&mut media, opt.settings(), name, kind).await?;
    let mut packer = Packer::new(&learned, opt.format)?;

    for frame in &learned.backlog {
        if let Some(chunk) = packer.frame(frame) {
            fanout.push(&chunk);
        }
    }

    while let Some(frame) = media.recv().await {
        if let Some(chunk) = packer.frame(&frame) {
            fanout.push(&chunk);
        }
        if fanout.consumers() == 0 {
            return Ok(());
        }
    }
    Err(anyhow!("the camera stopped sending"))
}

/// Serve a camera on a named pipe, one reader at a time.
#[cfg(unix)]
async fn serve_fifo(path: PathBuf, name: String, fanout: Arc<Fanout>) -> Result<()> {
    fifo::create(&path).with_context(|| format!("Could not create the FIFO {path:?}"))?;
    info!("{name}: serving {path:?}");

    loop {
        // Returns as soon as a reader opens the other end, and not
        // before — which is exactly the signal we want, since it is also
        // when the camera should be connected.
        let pipe = fifo::open_when_read(&path).await?;
        debug!("{name}: reader attached to {path:?}");

        let consumer = fanout.attach();
        match fifo::write_consumer(&consumer, pipe).await {
            Ok(()) => debug!("{name}: reader finished with {path:?}"),
            Err(e) if is_disconnect(&e) => debug!("{name}: reader left {path:?}"),
            Err(e) => warn!("{name}: error writing {path:?}: {e:?}"),
        }
        report_overruns(&name, "fifo", consumer.overruns());
        // Round we go: the consumer is dropped here, which detaches it,
        // and if it was the last one the camera is released.
    }
}

#[cfg(not(unix))]
async fn serve_fifo(_path: PathBuf, _name: String, _fanout: Arc<Fanout>) -> Result<()> {
    Err(anyhow!("Named pipes are only supported on unix"))
}

/// Serve a camera on a unix socket, to as many readers as turn up.
#[cfg(unix)]
async fn serve_socket(path: PathBuf, name: String, fanout: Arc<Fanout>) -> Result<()> {
    // A socket left behind by a previous run would make bind fail with
    // "address in use" even though nothing is listening on it.
    if tokio::fs::symlink_metadata(&path).await.is_ok() {
        tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("Could not replace the stale socket {path:?}"))?;
    }
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("Could not bind the socket {path:?}"))?;
    info!("{name}: serving {path:?}");

    loop {
        let (socket, _) = listener
            .accept()
            .await
            .with_context(|| format!("Could not accept on {path:?}"))?;
        debug!("{name}: reader connected to {path:?}");

        // Unlike the FIFO, several readers can be served at once; each
        // gets its own consumer and so its own keyframe start.
        let consumer = fanout.attach();
        let thread_name = name.clone();
        let thread_path = path.clone();
        tokio::spawn(async move {
            match write_consumer(&consumer, socket).await {
                Ok(()) => debug!("{thread_name}: reader finished with {thread_path:?}"),
                Err(e) if is_disconnect(&e) => {
                    debug!("{thread_name}: reader left {thread_path:?}")
                }
                Err(e) => warn!("{thread_name}: error writing {thread_path:?}: {e:?}"),
            }
            report_overruns(&thread_name, "socket", consumer.overruns());
        });
    }
}

#[cfg(not(unix))]
async fn serve_socket(_path: PathBuf, _name: String, _fanout: Arc<Fanout>) -> Result<()> {
    Err(anyhow!("Unix sockets are only supported on unix"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::stream::fanout::Chunk;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixStream;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "neolink-pipe-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("could not make a scratch directory");
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Connect, and wait for the server to have registered us — otherwise
    /// a push can race ahead of the accept and make the test flaky.
    async fn join(path: &PathBuf, fanout: &Fanout, expect: usize) -> UnixStream {
        let socket = UnixStream::connect(path).await.expect("should connect");
        for _ in 0..200 {
            if fanout.consumers() >= expect {
                return socket;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("the server never registered the reader");
    }

    async fn read_exactly(socket: &mut UnixStream, count: usize) -> Vec<u8> {
        let mut got = vec![0u8; count];
        tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut got))
            .await
            .expect("reader should not time out")
            .expect("reader should not error");
        got
    }

    #[tokio::test]
    async fn the_socket_serves_several_readers_from_one_camera() {
        let scratch = Scratch::new("multi");
        let path = scratch.path("Front.sock");
        let fanout = Arc::new(Fanout::new(None));

        let server = tokio::spawn(serve_socket(
            path.clone(),
            "Front".to_string(),
            fanout.clone(),
        ));
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        // A GOP is already in flight before anybody connects.
        fanout.push(&Chunk::new(vec![b'K'; 8], true, 0));
        fanout.push(&Chunk::new(vec![b'a'; 8], false, 40_000));

        // Three readers, joining at different points in that GOP.
        let mut first = join(&path, &fanout, 1).await;
        fanout.push(&Chunk::new(vec![b'b'; 8], false, 80_000));
        let mut second = join(&path, &fanout, 2).await;
        fanout.push(&Chunk::new(vec![b'c'; 8], false, 120_000));
        let mut third = join(&path, &fanout, 3).await;
        fanout.push(&Chunk::new(vec![b'd'; 8], false, 160_000));

        // Each gets the whole GOP from its keyframe, not a slice of the
        // bytes and not a wait for the next one.
        let expected: Vec<u8> = b"KKKKKKKKaaaaaaaabbbbbbbbccccccccdddddddd".to_vec();
        for (who, socket) in [
            ("first", &mut first),
            ("second", &mut second),
            ("third", &mut third),
        ] {
            assert_eq!(
                read_exactly(socket, expected.len()).await,
                expected,
                "{who} reader should get a complete stream of its own"
            );
        }

        assert_eq!(fanout.consumers(), 3, "all three should still be attached");
        server.abort();
    }

    #[tokio::test]
    async fn a_reader_leaving_does_not_disturb_the_others() {
        let scratch = Scratch::new("leave");
        let path = scratch.path("Front.sock");
        let fanout = Arc::new(Fanout::new(None));

        let server = tokio::spawn(serve_socket(
            path.clone(),
            "Front".to_string(),
            fanout.clone(),
        ));
        for _ in 0..200 {
            if path.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }

        fanout.push(&Chunk::new(vec![b'K'; 8], true, 0));
        let mut stays = join(&path, &fanout, 1).await;
        let leaves = join(&path, &fanout, 2).await;

        assert_eq!(read_exactly(&mut stays, 8).await, vec![b'K'; 8]);
        drop(leaves);

        // The camera must stay connected for the reader still watching.
        fanout.push(&Chunk::new(vec![b'e'; 8], false, 40_000));
        assert_eq!(read_exactly(&mut stays, 8).await, vec![b'e'; 8]);

        for _ in 0..200 {
            if fanout.consumers() == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            fanout.consumers(),
            1,
            "the departed reader should be forgotten, the remaining one kept"
        );
        server.abort();
    }
}
