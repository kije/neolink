use super::BcSubscription;
use crate::bc_protocol::battery_lifecycle::BatteryLifecycle;
use crate::{bc::model::*, Error, Result};
use futures::future::BoxFuture;
use futures::sink::{Sink, SinkExt};
use futures::stream::{Stream, StreamExt};
use log::*;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::watch;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use tokio::{sync::RwLock, task::JoinSet};

type MsgHandler = dyn 'static + Send + Sync + for<'a> Fn(&'a Bc) -> BoxFuture<'a, Option<Bc>>;

#[derive(Default)]
struct Subscriber {
    /// Subscribers based on their ID and their num
    /// First filtered by ID then number
    /// If num is None it will be upgraded to a Some based on the number the
    /// camera assigns
    num: BTreeMap<u32, BTreeMap<Option<u16>, Sender<Result<Bc>>>>,
    /// Subscribers based on their ID
    id: BTreeMap<u32, Arc<MsgHandler>>,
}

pub(crate) type BcConnSink = Box<dyn Sink<Bc, Error = Error> + Send + Sync + Unpin>;
pub(crate) type BcConnSource = Box<dyn Stream<Item = Result<Bc>> + Send + Sync + Unpin>;

/// A shareable connection to a camera.  Handles serialization of messages.  To send/receive, call
/// .[subscribe()] with a message number.  You can use the BcSubscription to send or receive only
/// messages with that number; each incoming message is routed to its appropriate subscriber.
///
/// There can be only one subscriber per kind of message at a time.
///
/// # `mess_id` channel packing (audit)
///
/// `reolink_aio` packs `mess_id = (ch_id << 24) | counter` on the wire so
/// that a single TCP socket shared between channels can route replies to
/// per-channel futures. In neolink each `BcCamera` (and therefore each
/// channel) owns its own `BcConnection` and TCP socket, so the dispatcher
/// only needs to key on `(msg_id, msg_num)` — `channel_id` is carried in
/// the header for the camera's benefit (NVR routing) but is not used for
/// response routing on this side. Keying on the `(msg_id, msg_num)` pair is
/// equivalent to `reolink_aio`'s `mess_id` scheme under this constraint.
pub struct BcConnection {
    sink: Sender<Result<Bc>>,
    poll_commander: Sender<PollCommand>,
    rx_thread: RwLock<JoinSet<Result<()>>>,
    cancel: CancellationToken,
    /// Shared with [`BcCamera`] and [`BcSubscription`] so subscriptions
    /// can call `track()` on send and the idle-monitor task can call
    /// `wait_idle()`.
    battery_lifecycle: Arc<BatteryLifecycle>,
    /// Observable "should-close" signal driven by the idle-monitor task.
    ///
    /// The actual TCP-socket close + reconnect-on-demand is a non-trivial
    /// architectural refactor (the `Arc<BcConnection>` field on `BcCamera`
    /// would need to be replaceable on the fly). Until that lands, the
    /// idle-monitor task drives this watch — any downstream subsystem
    /// (RTSP server, stream pumps, etc.) that wants to be told "the camera
    /// has been idle long enough for a battery close" can subscribe.
    idle_close_intent: watch::Receiver<bool>,
}

impl BcConnection {
    pub async fn new(
        mut sink: BcConnSink,
        mut source: BcConnSource,
        battery_lifecycle: Arc<BatteryLifecycle>,
    ) -> Result<BcConnection> {
        let (sinker, sinker_rx) = channel::<Result<Bc>>(100);
        let cancel = CancellationToken::new();

        let (poll_commander, poll_commanded) = channel(200);
        let mut poller = Poller {
            subscribers: Default::default(),
            sink: sinker.clone(),
            reciever: ReceiverStream::new(poll_commanded),
        };

        let mut rx_thread = JoinSet::<Result<()>>::new();
        let thread_poll_commander = poll_commander.clone();
        let thread_cancel = cancel.clone();
        rx_thread.spawn(async move {
            tokio::select! {
                _ = thread_cancel.cancelled() => {
                    Result::Ok(())
                },
                v = async {
                    let sender = thread_poll_commander;
                    while let Some(bc) = source.next().await {
                        sender.send(PollCommand::Bc(Box::new(bc))).await?;
                    }
                    Result::Ok(())
                } => v
            }
        });

        let thread_cancel = cancel.clone();
        rx_thread.spawn(async move {
            tokio::select! {
                _ = thread_cancel.cancelled() => Result::Ok(()),
                v = async {
                    let mut stream = ReceiverStream::new(sinker_rx);
                    while let Some(packet) = stream.next().await {
                        sink.send(packet?).await?;
                    }
                    Ok(())
                } => v
            }
        });

        let thread_cancel = cancel.clone();
        rx_thread.spawn(async move {
            tokio::select! {
                _ = thread_cancel.cancelled() => Result::Ok(()),
                v = async {
                    loop {
                        if let n @ Err(_) = poller.run().await {
                            trace!("Polling has ended");
                            return n;
                        }
                    }
                }=> v
            }
        });

        // Idle-monitor task: while battery-managed, wait for the in-flight
        // count to next hit zero, sleep for the configured idle window, and
        // if still idle, emit a `true` on `idle_close_intent`. Any consumer
        // that wants to drive the actual TCP close subscribes to the watch.
        let (idle_tx, idle_rx) = watch::channel(false);
        let idle_lc = battery_lifecycle.clone();
        let thread_cancel = cancel.clone();
        rx_thread.spawn(async move {
            tokio::select! {
                _ = thread_cancel.cancelled() => Result::Ok(()),
                v = async {
                    loop {
                        // Cheap spin gate: do nothing unless this is a
                        // battery camera.
                        if !idle_lc.is_enabled() {
                            tokio::time::sleep(idle_lc.idle_window()).await;
                            continue;
                        }
                        idle_lc.wait_idle().await;
                        // Race the idle window against the next command.
                        let still_idle = tokio::select! {
                            _ = tokio::time::sleep(idle_lc.idle_window()) => true,
                            // Re-arm the moment the in-flight count goes
                            // back up; wait_idle() returns only on the
                            // next 1 -> 0 transition.
                            _ = async {
                                // Naive readiness poll for "in_flight > 0
                                // again". A condvar-like API on the
                                // lifecycle would be cleaner; in practice
                                // this branch runs at most once per
                                // idle-window and the loop just rearms.
                                loop {
                                    if idle_lc.in_flight() > 0 {
                                        break;
                                    }
                                    tokio::time::sleep(
                                        std::time::Duration::from_millis(50)
                                    ).await;
                                }
                            } => false,
                        };
                        if still_idle {
                            log::debug!(
                                "Battery camera idle for {:?} — emitting close-intent",
                                idle_lc.idle_window()
                            );
                            let _ = idle_tx.send(true);
                            // Hold the signal until something flips
                            // in_flight back up; clear once activity
                            // resumes so downstream subscribers see a
                            // clean false -> true edge next time.
                            loop {
                                if idle_lc.in_flight() > 0 {
                                    let _ = idle_tx.send(false);
                                    break;
                                }
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(100)
                                ).await;
                            }
                        }
                    }
                } => v,
            }
        });

        Ok(BcConnection {
            sink: sinker,
            poll_commander,
            rx_thread: RwLock::new(rx_thread),
            cancel,
            battery_lifecycle,
            idle_close_intent: idle_rx,
        })
    }

    /// Returns the shared battery-lifecycle handle.
    ///
    /// `BcSubscription::send` uses this to call `track()` so the in-flight
    /// count reflects real commands in production, not just unit tests.
    pub(crate) fn battery_lifecycle(&self) -> &Arc<BatteryLifecycle> {
        &self.battery_lifecycle
    }

    /// Subscribe to the idle-close intent signal. Becomes `true` whenever
    /// a battery camera has been idle for [`BatteryLifecycle::idle_window`],
    /// and back to `false` as soon as a tracked command lands again.
    #[allow(dead_code)] // wired up by callers that want to react to idle
    pub fn watch_idle_close_intent(&self) -> watch::Receiver<bool> {
        self.idle_close_intent.clone()
    }

    pub(super) async fn send(&self, bc: Bc) -> crate::Result<()> {
        self.sink.send(Ok(bc)).await?;
        Ok(())
    }

    pub async fn subscribe(&self, msg_id: u32, msg_num: u16) -> Result<BcSubscription> {
        let (tx, rx) = channel(100);
        self.poll_commander
            .send(PollCommand::AddSubscriber(msg_id, Some(msg_num), tx))
            .await?;
        Ok(BcSubscription::new(rx, Some(msg_num as u32), self))
    }

    /// Some messages are initiated by the camera. This creates a handler for them
    /// It requires a closure that will be used to handle the message
    /// and return either None or Some(Bc) reply
    pub async fn handle_msg<T>(&self, msg_id: u32, handler: T) -> Result<()>
    where
        T: 'static + Send + Sync + for<'a> Fn(&'a Bc) -> BoxFuture<'a, Option<Bc>>,
    {
        self.poll_commander
            .send(PollCommand::AddHandler(msg_id, Arc::new(handler)))
            .await?;
        Ok(())
    }

    /// Stop a message handler created using [`handle_msg`]
    #[allow(dead_code)] // Currently unused but added for future use
    pub async fn unhandle_msg(&self, msg_id: u32) -> Result<()> {
        self.poll_commander
            .send(PollCommand::RemoveHandler(msg_id))
            .await?;
        Ok(())
    }

    /// Some times we want to wait for a reply on a new message ID
    /// to do this we wait for the next packet with a certain ID
    /// grab it's message ID and then subscribe to that ID
    ///
    /// The command Snap that grabs a jpeg payload is an example of this
    ///
    /// This function creates a temporary handle to grab this single message
    pub async fn subscribe_to_id(&self, msg_id: u32) -> Result<BcSubscription> {
        let (tx, rx) = channel(100);
        self.poll_commander
            .send(PollCommand::AddSubscriber(msg_id, None, tx))
            .await?;
        Ok(BcSubscription::new(rx, None, self))
    }

    pub(crate) async fn join(&self) -> Result<()> {
        let mut locked_threads = self.rx_thread.write().await;
        while let Some(res) = locked_threads.join_next().await {
            match res {
                Err(e) => {
                    locked_threads.abort_all();
                    return Err(e.into());
                }
                Ok(Err(e)) => {
                    locked_threads.abort_all();
                    return Err(e);
                }
                Ok(Ok(())) => {}
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.poll_commander.send(PollCommand::Disconnect).await;
        self.cancel.cancel();
        let mut locked_threads = self.rx_thread.write().await;
        while locked_threads.join_next().await.is_some() {}
        Ok(())
    }
}

impl Drop for BcConnection {
    fn drop(&mut self) {
        log::trace!("Drop BcConnection");
        self.cancel.cancel();

        let poll_commander = self.poll_commander.clone();
        let _gt = tokio::runtime::Handle::current().enter();
        let mut threads = std::mem::take(&mut self.rx_thread);
        tokio::task::spawn(async move {
            let _ = poll_commander.send(PollCommand::Disconnect).await;
            let locked_threads = threads.get_mut();
            while locked_threads.join_next().await.is_some() {}
            log::trace!("Dropped BcConnection");
        });
    }
}

enum PollCommand {
    Bc(Box<Result<Bc>>),
    AddHandler(u32, Arc<MsgHandler>),
    RemoveHandler(u32),
    AddSubscriber(u32, Option<u16>, Sender<Result<Bc>>),
    Disconnect,
}

impl std::fmt::Debug for PollCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PollCommand::Bc(_) => f.write_str("PollCommand::Bc"),
            PollCommand::AddHandler(_, _) => f.write_str("PollCommand::AddHandler"),
            PollCommand::RemoveHandler(_) => f.write_str("PollCommand::RemoveHandler"),
            PollCommand::AddSubscriber(_, _, _) => f.write_str("PollCommand::AddSubscriber"),
            PollCommand::Disconnect => f.write_str("PollCommand::Disconnect"),
        }
    }
}

struct Poller {
    subscribers: Subscriber,
    sink: Sender<Result<Bc>>,
    reciever: ReceiverStream<PollCommand>,
}

impl Poller {
    async fn run(&mut self) -> Result<()> {
        let cancel = CancellationToken::new();
        let _dropguard = cancel.clone().drop_guard();
        while let Some(command) = self.reciever.next().await {
            // Clean Up subscribers
            self.subscribers
                .num
                .iter_mut()
                .for_each(|(_, channels)| channels.retain(|_, channel| !channel.is_closed()));
            self.subscribers
                .num
                .retain(|_, channels| !channels.is_empty());
            // Handle the command
            match command {
                PollCommand::Bc(boxed_response) => {
                    match *boxed_response {
                        Ok(response) => {
                            let msg_id = response.meta.msg_id;
                            let msg_num = response.meta.msg_num;
                            log::trace!(
                                "Looking for ID: {} with num: {}, in {:?} and {:?}",
                                msg_id,
                                msg_num,
                                self.subscribers.id.keys().to_owned(),
                                self.subscribers
                                    .num
                                    .iter()
                                    .map(|(k, v)| (k, v.keys()))
                                    .collect::<Vec<_>>(),
                            );
                            match (
                                self.subscribers.id.get(&msg_id),
                                self.subscribers.num.get_mut(&msg_id), // Both filter first on ID
                            ) {
                                (Some(occ), _) => {
                                    log::trace!("Calling ID callback");
                                    let occ = occ.clone();
                                    let sink = self.sink.clone();
                                    // Move this on another thread coz I have NO idea
                                    // how long the callback will run for
                                    // and we must NOT hang
                                    let cancel = cancel.clone();
                                    tokio::task::spawn(async move {
                                        tokio::select! {
                                            _ = cancel.cancelled() => Result::Ok(()),
                                            v = occ(&response) => {
                                                if let Some(reply) = v {
                                                    assert!(reply.meta.msg_num == response.meta.msg_num);
                                                    sink.send(Ok(reply)).await?;
                                                }
                                                Result::Ok(())
                                            }
                                        }
                                    });
                                    log::trace!("Called ID callback");
                                }
                                (None, Some(occ)) => {
                                    let sender = if let Some(sender) =
                                        occ.get(&Some(msg_num)).filter(|a| !a.is_closed()).cloned()
                                    {
                                        // Connection with id exists and is not closed
                                        Some(sender)
                                    } else if let Some(sender) = occ.get(&None).cloned() {
                                        // Upgrade a None to a known MsgID
                                        occ.remove(&None);
                                        occ.insert(Some(msg_num), sender.clone());
                                        Some(sender)
                                    } else if occ
                                        .get(&Some(msg_num))
                                        .map(|a| a.is_closed())
                                        .unwrap_or(false)
                                    {
                                        // Connection is closed and there is no None to replace it
                                        // Remove it for cleanup and report no sender
                                        occ.remove(&Some(msg_num));
                                        None
                                    } else {
                                        None
                                    };
                                    if let Some(sender) = sender {
                                        if sender.capacity() == 0 {
                                            warn!("Reaching limit of channel");
                                            warn!(
                                                "Remaining: {} of {} message space for {} (ID: {})",
                                                sender.capacity(),
                                                sender.max_capacity(),
                                                &msg_num,
                                                &msg_id
                                            );
                                        } else {
                                            trace!(
                                                "Remaining: {} of {} message space for {} (ID: {})",
                                                sender.capacity(),
                                                sender.max_capacity(),
                                                &msg_num,
                                                &msg_id
                                            );
                                        }
                                        let _ = sender.send(Ok(response)).await;
                                    } else {
                                        trace!(
                                            "Ignoring uninteresting message id {} (number: {})",
                                            msg_id,
                                            msg_num
                                        );
                                        trace!("Contents: {:?}", response);
                                    }
                                }
                                (None, None) => {
                                    trace!(
                                        "Ignoring uninteresting message id {} (number: {})",
                                        msg_id,
                                        msg_num
                                    );
                                    trace!("Contents: {:?}", response);
                                }
                            }
                        }
                        Err(e) => {
                            for sub in self.subscribers.num.values() {
                                for sender in sub.values() {
                                    let _ = sender.send(Err(e.clone())).await;
                                }
                            }
                            self.subscribers.num.clear();
                            self.subscribers.id.clear();
                            return Err(e);
                        }
                    }
                }
                PollCommand::AddHandler(msg_id, handler) => {
                    match self.subscribers.id.entry(msg_id) {
                        Entry::Vacant(vac_entry) => {
                            vac_entry.insert(handler);
                        }
                        Entry::Occupied(_) => {
                            return Err(Error::SimultaneousSubscriptionId { msg_id });
                        }
                    };
                }
                PollCommand::RemoveHandler(msg_id) => {
                    self.subscribers.id.remove(&msg_id);
                }
                PollCommand::AddSubscriber(msg_id, msg_num, tx) => {
                    match self
                        .subscribers
                        .num
                        .entry(msg_id)
                        .or_default()
                        .entry(msg_num)
                    {
                        Entry::Vacant(vac_entry) => {
                            vac_entry.insert(tx);
                        }
                        Entry::Occupied(mut occ_entry) => {
                            if occ_entry.get().is_closed() {
                                occ_entry.insert(tx);
                            } else {
                                // log::error!("Failed to subscribe in bcconn to {:?} for {:?}", msg_num, msg_id);
                                let _ = tx
                                    .send(Err(Error::SimultaneousSubscription { msg_num }))
                                    .await;
                            }
                        }
                    };
                }
                PollCommand::Disconnect => {
                    return Err(Error::ConnectionShutdown);
                }
            }
        }
        Ok(())
    }
}
