//! Getting muxed bytes from one camera to however many readers want them,
//! without letting a slow one hold up the rest — or the camera.
//!
//! A live stream has a property a file does not: old data is worthless. If
//! a reader stalls, the useful thing to send it when it comes back is
//! whatever is happening *now*, not the backlog of what it missed. Queue
//! that backlog instead and two things go wrong at once — memory grows for
//! as long as the stall lasts, and when the reader recovers it plays the
//! stall back, permanently that far behind real time.
//!
//! So each consumer gets a bounded queue measured in *stream time*, not
//! bytes, and when it overruns the queue is thrown away rather than
//! trimmed. Throwing it away is the only correct move: the frames in it are
//! deltas against each other, so a consumer cannot simply be handed the
//! newest one. It has to wait for the next keyframe, which is what
//! [`Chunk::resync_point`] marks.
//!
//! The producer never blocks and never awaits. [`Fanout::push`] takes each
//! consumer's lock just long enough to append or discard, so one reader
//! blocked on a full pipe cannot stop the camera being read, and cannot
//! stop a second reader that is keeping up perfectly well.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio::time::Duration;

/// One frame's worth of muxed output, ready to write.
#[derive(Clone)]
pub(crate) struct Chunk {
    /// The bytes. Shared, so fanning out to N consumers copies nothing.
    pub(crate) bytes: Arc<[u8]>,
    /// Whether a reader can start here — that is, whether these bytes
    /// open with a PAT/PMT and a keyframe. A consumer that has just
    /// attached, or has just thrown away a backlog, waits for one.
    pub(crate) resync_point: bool,
    /// Presentation time of the frame in microseconds, used to measure
    /// how far behind a consumer has fallen. Only differences matter.
    pub(crate) pts_us: u64,
}

impl Chunk {
    pub(crate) fn new(bytes: Vec<u8>, resync_point: bool, pts_us: u64) -> Self {
        Self {
            bytes: Arc::from(bytes),
            resync_point,
            pts_us,
        }
    }
}

/// The queue behind one consumer.
struct Queue {
    chunks: VecDeque<Chunk>,
    /// Set while we are waiting for a keyframe: either the consumer has
    /// only just attached, or its backlog was dropped.
    awaiting_resync: bool,
    /// How many chunks at the front are the catch-up burst handed over
    /// at attach time.
    ///
    /// They are already old by definition — a whole GOP of them — so
    /// measuring "how far behind" from there would declare a reader
    /// stalled the moment it connected. The backlog check skips them and
    /// starts from the first genuinely live chunk instead.
    seeded: usize,
    /// Set when the producer has finished for good.
    closed: bool,
    /// How many times this consumer's backlog has been thrown away.
    overruns: u64,
}

struct Inner {
    queue: Mutex<Queue>,
    wake: Notify,
}

/// A reader's end of a [`Fanout`].
///
/// Dropping it detaches from the fanout; there is nothing to call.
pub(crate) struct Consumer {
    inner: Arc<Inner>,
}

impl Consumer {
    /// The next chunk to write, or `None` once the producer has finished
    /// and the queue is drained.
    pub(crate) async fn next(&self) -> Option<Chunk> {
        loop {
            // Register for the wake-up *before* looking at the queue, so
            // a push landing between the two cannot be missed.
            let wake = self.inner.wake.notified();
            {
                let mut queue = self.lock();
                if let Some(chunk) = queue.chunks.pop_front() {
                    queue.seeded = queue.seeded.saturating_sub(1);
                    return Some(chunk);
                }
                if queue.closed {
                    return None;
                }
            }
            wake.await;
        }
    }

    /// How many times this consumer has fallen far enough behind that its
    /// backlog was discarded.
    pub(crate) fn overruns(&self) -> u64 {
        self.lock().overruns
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.inner.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Most a cached GOP may occupy before we give up on caching it.
///
/// The cache exists so a reader joining an already-running stream can
/// start now instead of waiting for the next keyframe, which on a camera
/// with a four second GOP is four seconds of nothing. That is worth a few
/// megabytes; it is not worth unbounded memory if a camera turns out to
/// have an enormous keyframe interval, so past this we stop caching and
/// the next reader waits as it used to.
const GOP_CACHE_LIMIT: usize = 16 * 1024 * 1024;

/// The most recent GOP, kept so a new reader can start immediately.
#[derive(Default)]
struct Gop {
    chunks: Vec<Chunk>,
    bytes: usize,
    /// Whether what is held is a whole GOP from its keyframe. False after
    /// the cap is hit, until the next keyframe starts a fresh one — a
    /// partial GOP is worse than none, since it would hand a decoder
    /// frames whose references it never saw.
    whole: bool,
}

/// State that has to move together.
///
/// The consumer list and the cached GOP share one lock rather than
/// having one each, because attaching has to read the cache and join the
/// list as a single step. Split them and a chunk pushed in between would
/// reach the existing readers and the cache but not the new reader,
/// leaving it a gap exactly where it started.
#[derive(Default)]
struct State {
    consumers: Vec<Weak<Inner>>,
    gop: Gop,
}

/// The producer's end: one camera's muxed output, shared out to readers.
pub(crate) struct Fanout {
    state: Mutex<State>,
    /// How much stream time a consumer may have queued before its
    /// backlog is discarded. `None` never discards, which is what a
    /// recording to a file wants and a live reader does not.
    limit_us: Option<u64>,
    /// Notified when a consumer attaches.
    attached: Notify,
}

impl Fanout {
    pub(crate) fn new(limit: Option<Duration>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            limit_us: limit.map(|d| d.as_micros().min(u64::MAX as u128) as u64),
            attached: Notify::new(),
        }
    }

    /// Attach a reader.
    ///
    /// Whatever it sees first is a PAT/PMT and an I-frame, never the
    /// middle of a GOP. Where possible that is the GOP already in
    /// progress, handed over at once so the reader has a picture
    /// immediately; otherwise it waits for the next keyframe.
    pub(crate) fn attach(&self) -> Consumer {
        let mut state = self.lock();

        let seed: VecDeque<Chunk> = if state.gop.whole {
            state.gop.chunks.iter().cloned().collect()
        } else {
            VecDeque::new()
        };
        let awaiting_resync = seed.is_empty();

        let inner = Arc::new(Inner {
            queue: Mutex::new(Queue {
                seeded: seed.len(),
                chunks: seed,
                awaiting_resync,
                closed: false,
                overruns: 0,
            }),
            wake: Notify::new(),
        });
        state.consumers.push(Arc::downgrade(&inner));
        drop(state);

        self.attached.notify_waiters();
        Consumer { inner }
    }

    /// Hand a chunk to every attached reader, dropping backlogs that have
    /// grown past the limit. Never blocks and never awaits.
    pub(crate) fn push(&self, chunk: &Chunk) {
        let mut state = self.lock();

        // Keep the GOP in progress, so the next reader to turn up does
        // not have to wait for the keyframe after this one.
        let gop = &mut state.gop;
        if chunk.resync_point {
            gop.chunks.clear();
            gop.bytes = 0;
            gop.whole = true;
        }
        if gop.whole {
            gop.bytes += chunk.bytes.len();
            if gop.bytes > GOP_CACHE_LIMIT {
                gop.chunks.clear();
                gop.bytes = 0;
                gop.whole = false;
            } else {
                gop.chunks.push(chunk.clone());
            }
        }

        // Detached readers are pruned here rather than on drop, which is
        // what lets `Consumer` have no teardown of its own.
        state.consumers.retain(|weak| {
            let Some(inner) = weak.upgrade() else {
                return false;
            };
            let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());

            if queue.awaiting_resync {
                if !chunk.resync_point {
                    return true;
                }
                queue.awaiting_resync = false;
            }

            if let Some(limit) = self.limit_us {
                // Skip the catch-up burst: it is old on purpose, and
                // counting it would declare a reader stalled the instant
                // it connected.
                let behind = queue
                    .chunks
                    .get(queue.seeded)
                    .map(|front| chunk.pts_us.saturating_sub(front.pts_us))
                    .unwrap_or(0);
                if behind > limit {
                    // This reader is not keeping up. Everything queued
                    // for it is now stale, so drop the lot and start it
                    // again at the next keyframe rather than making it
                    // replay the stall.
                    queue.chunks.clear();
                    queue.seeded = 0;
                    queue.overruns += 1;
                    if !chunk.resync_point {
                        queue.awaiting_resync = true;
                        return true;
                    }
                }
            }

            queue.chunks.push_back(chunk.clone());
            drop(queue);
            inner.wake.notify_one();
            true
        });
    }

    /// How many readers are attached, pruning any that have gone.
    pub(crate) fn consumers(&self) -> usize {
        let mut state = self.lock();
        state.consumers.retain(|weak| weak.strong_count() > 0);
        state.consumers.len()
    }

    /// Wait until at least one reader is attached.
    ///
    /// This is what keeps the camera off the wire while nobody is
    /// watching: the producer parks here rather than connecting.
    pub(crate) async fn wait_for_consumer(&self) {
        loop {
            let attached = self.attached.notified();
            if self.consumers() > 0 {
                return;
            }
            attached.await;
        }
    }

    /// Tell every reader that no more chunks are coming, so their `next`
    /// returns `None` once they have drained what they have.
    pub(crate) fn close(&self) {
        let drained: Vec<Weak<Inner>> = self.lock().consumers.drain(..).collect();
        for weak in drained {
            if let Some(inner) = weak.upgrade() {
                inner.queue.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
                inner.wake.notify_one();
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(pts_ms: u64, resync: bool) -> Chunk {
        Chunk::new(vec![pts_ms as u8; 16], resync, pts_ms * 1_000)
    }

    /// Drain everything currently queued, without waiting.
    async fn drain(consumer: &Consumer) -> Vec<u64> {
        let mut seen = Vec::new();
        while let Ok(Some(chunk)) =
            tokio::time::timeout(Duration::from_millis(20), consumer.next()).await
        {
            seen.push(chunk.pts_us / 1_000);
        }
        seen
    }

    #[tokio::test]
    async fn a_new_consumer_starts_at_a_keyframe() {
        let fanout = Fanout::new(None);
        let consumer = fanout.attach();

        // Mid-GOP when it attaches: these have to be skipped, or the
        // reader gets frames its decoder has no reference for.
        fanout.push(&chunk(0, false));
        fanout.push(&chunk(40, false));
        fanout.push(&chunk(80, true));
        fanout.push(&chunk(120, false));

        assert_eq!(drain(&consumer).await, vec![80, 120]);
    }

    #[tokio::test]
    async fn a_stalled_consumer_loses_its_backlog_rather_than_replaying_it() {
        let fanout = Fanout::new(Some(Duration::from_millis(100)));
        let consumer = fanout.attach();

        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));
        fanout.push(&chunk(80, false));
        // 200ms of stream time queued, past the 100ms limit: the backlog
        // goes, and this frame is not a keyframe so nothing is queued.
        fanout.push(&chunk(200, false));
        assert_eq!(consumer.overruns(), 1);

        // Still nothing until the stream offers somewhere to restart.
        fanout.push(&chunk(240, false));
        fanout.push(&chunk(280, true));
        fanout.push(&chunk(320, false));

        assert_eq!(
            drain(&consumer).await,
            vec![280, 320],
            "a recovered reader should see current video, not the stall"
        );
    }

    #[tokio::test]
    async fn an_overrun_on_a_keyframe_restarts_immediately() {
        let fanout = Fanout::new(Some(Duration::from_millis(100)));
        let consumer = fanout.attach();

        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));
        // Overruns and is itself a resync point, so there is no reason
        // to wait for another one.
        fanout.push(&chunk(200, true));

        assert_eq!(drain(&consumer).await, vec![200]);
        assert_eq!(consumer.overruns(), 1);
    }

    #[tokio::test]
    async fn no_limit_means_nothing_is_ever_dropped() {
        // What a recording to a file wants: fall behind, but lose nothing.
        let fanout = Fanout::new(None);
        let consumer = fanout.attach();

        fanout.push(&chunk(0, true));
        for i in 1..100u64 {
            fanout.push(&chunk(i * 1_000, false));
        }

        assert_eq!(drain(&consumer).await.len(), 100);
        assert_eq!(consumer.overruns(), 0);
    }

    #[tokio::test]
    async fn one_slow_consumer_does_not_affect_a_fast_one() {
        let fanout = Fanout::new(Some(Duration::from_millis(100)));
        let quick = fanout.attach();
        let slow = fanout.attach();

        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));
        // The quick one keeps up; the slow one leaves its queue alone.
        assert_eq!(drain(&quick).await, vec![0, 40]);

        fanout.push(&chunk(200, true));
        fanout.push(&chunk(240, false));

        assert_eq!(drain(&quick).await, vec![200, 240]);
        assert_eq!(
            drain(&slow).await,
            vec![200, 240],
            "the slow reader should resync, not stall the fast one"
        );
        assert_eq!(quick.overruns(), 0);
        assert_eq!(slow.overruns(), 1);
    }

    #[tokio::test]
    async fn a_detached_consumer_is_forgotten() {
        let fanout = Fanout::new(None);
        let consumer = fanout.attach();
        assert_eq!(fanout.consumers(), 1);

        drop(consumer);
        assert_eq!(fanout.consumers(), 0);

        // Pushing with nobody attached must not panic or leak.
        fanout.push(&chunk(0, true));
        assert_eq!(fanout.consumers(), 0);
    }

    #[tokio::test]
    async fn the_producer_waits_until_someone_is_reading() {
        let fanout = Arc::new(Fanout::new(None));

        // Nothing attached: this must not return.
        let waiting = fanout.clone();
        let handle = tokio::spawn(async move { waiting.wait_for_consumer().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "should still be waiting");

        let _consumer = fanout.attach();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("attaching a consumer should wake the producer")
            .unwrap();
    }

    #[tokio::test]
    async fn closing_ends_the_consumer_after_it_drains() {
        let fanout = Fanout::new(None);
        let consumer = fanout.attach();
        fanout.push(&chunk(0, true));
        fanout.close();

        assert!(
            consumer.next().await.is_some(),
            "queued data survives close"
        );
        assert!(consumer.next().await.is_none(), "then the stream ends");
    }

    #[tokio::test]
    async fn a_reader_joining_mid_stream_starts_at_once() {
        // Without the cached GOP this reader would see nothing until the
        // next keyframe — on a four second GOP, four seconds of black.
        let fanout = Fanout::new(Some(Duration::from_millis(500)));

        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));
        fanout.push(&chunk(80, false));

        let latecomer = fanout.attach();
        assert_eq!(
            drain(&latecomer).await,
            vec![0, 40, 80],
            "a new reader should get the GOP in progress, from its keyframe"
        );

        // And then carries on live.
        fanout.push(&chunk(120, false));
        assert_eq!(drain(&latecomer).await, vec![120]);
    }

    #[tokio::test]
    async fn the_catch_up_burst_is_not_mistaken_for_falling_behind() {
        // The seeded GOP is older than the backlog limit by construction.
        // Counting it would drop the reader's queue the instant it
        // connected, which is the bug this guards.
        let fanout = Fanout::new(Some(Duration::from_millis(100)));

        fanout.push(&chunk(0, true));
        for i in 1..=8u64 {
            fanout.push(&chunk(i * 40, false));
        }

        let latecomer = fanout.attach();
        fanout.push(&chunk(360, false));

        assert_eq!(latecomer.overruns(), 0, "connecting is not falling behind");
        assert_eq!(drain(&latecomer).await.len(), 10);
    }

    #[tokio::test]
    async fn a_reader_that_stalls_after_catching_up_is_still_caught() {
        // The exemption must not be permanent.
        let fanout = Fanout::new(Some(Duration::from_millis(100)));
        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));

        let latecomer = fanout.attach();
        // Drain the burst, then go quiet while the stream runs on.
        assert_eq!(drain(&latecomer).await, vec![0, 40]);

        fanout.push(&chunk(80, false));
        fanout.push(&chunk(400, false));
        assert_eq!(latecomer.overruns(), 1);
    }

    #[tokio::test]
    async fn an_oversized_gop_is_not_cached() {
        // A camera with an enormous keyframe interval must not be able to
        // grow the cache without bound; the reader waits instead.
        let fanout = Fanout::new(None);

        fanout.push(&Chunk::new(vec![0u8; 8], true, 0));
        let big = GOP_CACHE_LIMIT + 1;
        fanout.push(&Chunk::new(vec![0u8; big], false, 40_000));

        let latecomer = fanout.attach();
        fanout.push(&chunk(80, false));
        assert!(
            drain(&latecomer).await.is_empty(),
            "with no usable GOP the reader should wait for a keyframe"
        );

        fanout.push(&chunk(120, true));
        assert_eq!(drain(&latecomer).await, vec![120]);
    }

    #[tokio::test]
    async fn several_readers_joining_at_different_times_all_get_whole_gops() {
        let fanout = Fanout::new(Some(Duration::from_secs(5)));

        let first = fanout.attach();
        fanout.push(&chunk(0, true));
        fanout.push(&chunk(40, false));

        let second = fanout.attach();
        fanout.push(&chunk(80, false));

        let third = fanout.attach();
        fanout.push(&chunk(120, false));

        // Everyone starts on the keyframe and nobody misses a frame after
        // the point they joined.
        assert_eq!(drain(&first).await, vec![0, 40, 80, 120]);
        assert_eq!(drain(&second).await, vec![0, 40, 80, 120]);
        assert_eq!(drain(&third).await, vec![0, 40, 80, 120]);
        assert_eq!(fanout.consumers(), 3);
    }
}
