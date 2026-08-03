use crate::{
    config::{Config, MqttServerConfig},
    AnyResult,
};
use anyhow::{anyhow, Context, Result};
use futures::future::FutureExt;
use log::*;
use rumqttc::{
    AsyncClient, ConnectReturnCode, Event, Incoming, LastWill, MqttOptions, QoS, TlsConfiguration,
    Transport,
};
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio::{
    sync::{
        broadcast::{channel as broadcast, Sender as BroadcastSender},
        mpsc::{channel as mpsc, Receiver as MpscReceiver, Sender as MpscSender},
        oneshot::{channel as oneshot, Sender as OneshotSender},
        watch::Receiver as WatchReceiver,
    },
    time::{sleep, Duration, Instant},
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Reconnect backoff for the broker connection. A broker that is down or
/// refusing connections gets retried ever more slowly instead of being hammered
/// every two seconds for as long as it stays down.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A connection that lasted this long counts as healthy: the next failure
/// starts over from `RECONNECT_BACKOFF_MIN`.
const RECONNECT_BACKOFF_RESET: Duration = Duration::from_secs(60);

pub(crate) struct Mqtt {
    cancel: CancellationToken,
    outgoing_tx: MpscSender<MqttRequest>,
    set: JoinSet<Result<()>>,
}

impl Mqtt {
    pub(crate) async fn new(config: WatchReceiver<Config>) -> Self {
        let (incoming_tx, _) = broadcast::<MqttReply>(100);
        let (outgoing_tx, mut outgoing_rx) = mpsc::<MqttRequest>(100);
        let cancel = CancellationToken::new();
        let mut set = JoinSet::<AnyResult<()>>::new();

        // Thread that handles the mqttc side
        // including restarting it if the config changes
        let thread_cancel = cancel.clone();
        let mut thread_config = config;
        let thread_incoming_tx = incoming_tx;
        let thread_outgoing_tx = outgoing_tx.clone();
        let retry_cancel = thread_cancel.clone();
        set.spawn(async move {
            let mut mqtt_config = thread_config.borrow().mqtt.clone();
            let mut backoff = RECONNECT_BACKOFF_MIN;
            let r = loop {
                break tokio::select! {
                    _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                    v = thread_config.wait_for(|config| config.mqtt != mqtt_config).map(|res| res.map(|r| r.clone())) =>
                    {
                        mqtt_config = v?.mqtt.clone();
                        continue;
                    }
                    v = async {
                        let started = Instant::now();
                        let mut backend = MqttBackend {
                            incomming_tx: thread_incoming_tx.clone(),
                            outgoing_rx: &mut outgoing_rx,
                            outgoing_tx: thread_outgoing_tx.clone(),
                            config: mqtt_config.as_ref().unwrap(),
                            cancel: CancellationToken::new(),
                        };
                        (backend.run().await, started.elapsed())
                    }, if mqtt_config.is_some() => {
                        let (v, uptime) = v;
                        if let Err(e) = &v {
                            if uptime >= RECONNECT_BACKOFF_RESET {
                                backoff = RECONNECT_BACKOFF_MIN;
                            }
                            log::error!("MQTT Client Connection Failed: {:?}; retrying in {:?}", e, backoff);
                            tokio::select! {
                                _ = retry_cancel.cancelled() => {},
                                _ = sleep(backoff) => {},
                            }
                            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                            continue;
                        }
                        v
                    },
                };
            };
            log::debug!("MQTT thread stopped: {:?}", r);
            r
        });

        Self {
            cancel,
            outgoing_tx,
            set,
            // Send the drop message on clean disconnect
        }
    }

    pub async fn subscribe<T: Into<String>>(&self, name: T) -> AnyResult<MqttInstance> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(name.into(), tx))
            .await?;
        rx.await?
    }
}

impl Drop for Mqtt {
    fn drop(&mut self) {
        log::trace!("Drop MQTT");
        let outgoing_tx = self.outgoing_tx.clone();
        let cancel = self.cancel.clone();
        let mut set = std::mem::take(&mut self.set);

        let _gt = tokio::runtime::Handle::current().enter();
        tokio::task::spawn(async move {
            let (tx, rx) = oneshot();
            let _ = outgoing_tx.send(MqttRequest::HangUp(tx)).await;
            let _ = rx.await;

            log::debug!("Mqtt::drop Cancel");
            cancel.cancel();
            while set.join_next().await.is_some() {}
            log::trace!("Dropped MQTT");
        });
    }
}

struct MqttBackend<'a> {
    incomming_tx: BroadcastSender<MqttReply>,
    outgoing_rx: &'a mut MpscReceiver<MqttRequest>,
    outgoing_tx: MpscSender<MqttRequest>,
    config: &'a MqttServerConfig,
    cancel: CancellationToken,
}

impl<'a> MqttBackend<'a> {
    async fn run(&mut self) -> AnyResult<()> {
        log::trace!("Run MQTT Server");
        let mut mqttoptions = MqttOptions::new(
            format!("Neolink{}", Uuid::new_v4()),
            &self.config.broker_addr,
            self.config.port,
        );
        let max_size = 100 * (1024 * 1024);
        mqttoptions.set_max_packet_size(max_size, max_size);

        // Use TLS if ca path is set
        if let Some(ca_path) = &self.config.ca {
            if let Ok(ca) = std::fs::read(ca_path) {
                // Use client_auth if they have cert and key
                let client_auth = if let Some((cert_path, key_path)) = &self.config.client_auth {
                    if let (Ok(cert_buf), Ok(key_buf)) =
                        (std::fs::read(cert_path), std::fs::read(key_path))
                    {
                        Some((cert_buf, key_buf))
                    } else {
                        error!("Failed to set client tls");
                        None
                    }
                } else {
                    None
                };

                let transport = Transport::Tls(TlsConfiguration::Simple {
                    ca,
                    alpn: None,
                    client_auth,
                });
                mqttoptions.set_transport(transport);
            } else {
                error!("Failed to set CA");
            }
        };

        if let Some((username, password)) = &self.config.credentials {
            mqttoptions.set_credentials(username, password);
        }

        mqttoptions.set_keep_alive(Duration::from_secs(5));

        // On unclean disconnect send this
        mqttoptions.set_last_will(LastWill::new(
            "neolink/status".to_string(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, mut connection) = AsyncClient::new(mqttoptions, 100);

        let client = Arc::new(client);
        let send_client = client.clone();
        send_client
            .publish(
                "neolink/status".to_string(),
                QoS::AtLeastOnce,
                true,
                "connected".to_string(),
            )
            .await?;
        log::debug!("MQTT Published Startup");
        let loop_cancel = CancellationToken::new();
        let _drop_guard = loop_cancel.clone().drop_guard();
        loop {
            let r = tokio::select! {
                v = self.outgoing_rx.recv() => {
                    let msg = v.ok_or(anyhow!("All outgoing MQTT channels closed"))?;

                    match msg {
                        // Answered from local state, with no broker round trip
                        // at all, so answer it right here. Handing it to a task
                        // that races the backend teardown means a broker that
                        // is refusing connections can drop the reply channel
                        // instead, which the caller sees as a hard error and
                        // which used to take the whole daemon down with it.
                        MqttRequest::Subscribe(name, reply) => {
                            let instance = MqttInstance {
                                name,
                                incomming_rx: BroadcastStream::new(self.incomming_tx.subscribe()),
                                outgoing_tx: self.outgoing_tx.clone(),
                            };
                            let _ = reply.send(Ok(instance));
                        }
                        // `LastWillMqtt::new` registers the will on a connection
                        // of its own and returns without waiting for the broker,
                        // so this does not block polling for any meaningful time.
                        MqttRequest::LastWill{topic, message, reply} => {
                            let last_will = LastWillMqtt::new(
                                self.config,
                                topic,
                                message,
                            ).await;
                            let _ = reply.send(last_will);
                        }
                        MqttRequest::HangUp(reply) => {
                            // Best effort: if the broker is already gone there
                            // is nothing to say goodbye to, but the caller is
                            // waiting on this reply before it cancels us.
                            let _ = send_client.publish(
                                "neolink/status".to_string(),
                                QoS::AtLeastOnce,
                                true,
                                "disconnected".to_string(),
                            ).await;
                            let _ = reply.send(());
                        }
                        msg => {
                            // Publishing can block on the client's request queue,
                            // so put it on a task and keep polling the connection.
                            let outgoing_tx = self.outgoing_tx.clone();
                            let send_client = send_client.clone();
                            let cancel = self.cancel.clone();
                            let thread_cancel = loop_cancel.clone();
                            tokio::task::spawn(async move {
                                let mut pending = Some(msg);
                                tokio::select!{
                                    _ = cancel.cancelled() => {},
                                    _ = thread_cancel.cancelled() => {},
                                    _ = publish_request(&send_client, &mut pending) => {},
                                }
                                // Whatever the client did not take stays a whole
                                // request, reply channel included, and goes back
                                // on the queue for the next connection. Dropping
                                // it here would fail the sender for no better
                                // reason than that we happened to be tearing the
                                // backend down.
                                if let Some(msg) = pending {
                                    let _ = outgoing_tx.send(msg).await;
                                }
                            });
                        }
                    }

                    AnyResult::Ok(())
                },
                v = connection.poll() =>  {
                    let  notification = v.with_context(|| "MQTT connection dropped")?;
                    // Handle message on another thread so that we can keep polling
                    let client = client.clone();
                    let incomming_tx = self.incomming_tx.clone();
                    let cancel = self.cancel.clone();
                    let thread_cancel = loop_cancel.clone();
                    tokio::task::spawn(async move {
                        tokio::select!{
                            _ = cancel.cancelled() => AnyResult::Ok(()),
                            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                            v = async {
                                match notification {
                                    Event::Incoming(Incoming::ConnAck(connected)) => {
                                        if ConnectReturnCode::Success == connected.code {
                                            // Publish connected now that we are online
                                            client
                                            .publish(
                                                "neolink/status".to_string(),
                                                QoS::AtLeastOnce,
                                                true,
                                                "connected",
                                            )
                                            .await?;
                                            // We succesfully logged in. Now ask for the cameras subscription.
                                            client
                                            .subscribe("neolink/#".to_string(), QoS::AtMostOnce)
                                            .await?;
                                        }
                                    }
                                    Event::Incoming(Incoming::Publish(published_message)) => {
                                        if let Some(sub_topic) = published_message
                                            .topic
                                            .strip_prefix("neolink/")
                                        {
                                            let _ = incomming_tx
                                                .send(MqttReply {
                                                    topic: sub_topic.to_string(),
                                                    message: Arc::new(String::from_utf8_lossy(published_message.payload.as_ref())
                                                        .into_owned()),
                                                });
                                        }
                                    }
                                    _ => {}
                                }
                                AnyResult::Ok(())
                            } => v
                        }
                    });
                    AnyResult::Ok(())
                },
            };
            if r.is_ok() {
                continue;
            }
            break r;
        }?;
        Ok(())
    }
}

/// Hand a `Send`/`SendRetained` request to the MQTT client.
///
/// `pending` is only emptied once the client has accepted the message, and the
/// sender is only acknowledged at that point. If the client refuses it — which
/// is what happens once the connection behind it has gone away — the request is
/// left in `pending`, intact and still holding its reply channel, for the
/// caller to put back on the queue.
async fn publish_request(client: &AsyncClient, pending: &mut Option<MqttRequest>) {
    let (msg, retain) = match pending.as_ref() {
        Some(MqttRequest::Send(msg, _)) => (msg, false),
        Some(MqttRequest::SendRetained(msg, _)) => (msg, true),
        _ => return,
    };

    match client
        .publish(
            msg.topic.clone(),
            QoS::AtLeastOnce,
            retain,
            (*msg.message).clone(),
        )
        .await
    {
        Ok(()) => match pending.take() {
            Some(MqttRequest::Send(_, tx)) | Some(MqttRequest::SendRetained(_, tx)) => {
                let _ = tx.send(Ok(()));
            }
            _ => unreachable!("publish_request only takes a Send/SendRetained it just published"),
        },
        Err(e) => {
            log::debug!("MQTT publish deferred to the next connection: {e:?}");
        }
    }
}

impl Drop for MqttBackend<'_> {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct MqttInstance {
    outgoing_tx: MpscSender<MqttRequest>,
    incomming_rx: BroadcastStream<MqttReply>,
    name: String,
}

impl MqttInstance {
    pub(crate) fn get_name(&self) -> &str {
        &self.name
    }

    pub async fn subscribe<T: Into<String>>(&self, name: T) -> AnyResult<Self> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(name.into(), tx))
            .await?;
        rx.await?
    }

    pub async fn resubscribe(&self) -> AnyResult<Self> {
        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::Subscribe(self.name.clone(), tx))
            .await?;
        rx.await?
    }

    pub async fn send_message_with_root_topic(
        &self,
        root_topic: &str,
        sub_topic: &str,
        message: &str,
        retain: bool,
    ) -> AnyResult<()> {
        let topics = [
            root_topic.to_string(),
            self.name.clone(),
            sub_topic.to_string(),
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>();
        if retain {
            let (tx, rx) = oneshot();
            self.outgoing_tx
                .send(MqttRequest::SendRetained(
                    MqttReply {
                        topic: topics.join("/"),
                        message: Arc::new(message.to_string()),
                    },
                    tx,
                ))
                .await?;
            rx.await??;
        } else {
            let (tx, rx) = oneshot();
            self.outgoing_tx
                .send(MqttRequest::Send(
                    MqttReply {
                        topic: topics.join("/"),
                        message: Arc::new(message.to_string()),
                    },
                    tx,
                ))
                .await?;
            rx.await??;
        }
        Ok(())
    }

    pub async fn send_message(
        &self,
        sub_topic: &str,
        message: &str,
        retain: bool,
    ) -> AnyResult<()> {
        self.send_message_with_root_topic("neolink", sub_topic, message, retain)
            .await?;
        Ok(())
    }

    pub(crate) async fn recv(&mut self) -> AnyResult<MqttReply> {
        Ok(loop {
            let mut msg = self
                .incomming_rx
                .next()
                .await
                .ok_or(anyhow!("End of client data"))?
                .with_context(|| "Client stream is too far behind")?;
            // log::debug!("Got MQTT: {msg:?}");
            // log::debug!("self.name: {:?}", self.name);

            if self.name.is_empty() {
                break msg;
            } else {
                let mut topics = msg.topic.split('/');
                let sub_topic = topics.next();
                // log::debug!("topics: {:?}", msg.topic);
                // log::debug!("sub_topic: {sub_topic:?}");
                if sub_topic
                    .map(|subtopic| *subtopic == self.name)
                    .unwrap_or(false)
                {
                    msg.topic = topics.collect::<Vec<_>>().join("/");
                    // log::debug!("new topics: {:?}", msg.topic);
                    break msg;
                }
            }
        })
    }

    pub(crate) async fn last_will(&self, topic: &str, message: &str) -> AnyResult<LastWillMqtt> {
        let topic = if self.name.is_empty() {
            format!("neolink/{}", topic)
        } else {
            format!("neolink/{}/{}", self.name, topic)
        };

        let (tx, rx) = oneshot();
        self.outgoing_tx
            .send(MqttRequest::LastWill {
                topic,
                message: message.to_string(),
                reply: tx,
            })
            .await?;
        rx.await?
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MqttReply {
    pub(crate) topic: String,
    pub(crate) message: Arc<String>, // Messages can be long so avoid costly clones with an arc
}

impl MqttReply {
    pub(crate) fn as_ref(&self) -> MqttReplyRef {
        MqttReplyRef {
            topic: &self.topic,
            message: &self.message,
        }
    }
}

pub(crate) struct MqttReplyRef<'a> {
    pub(crate) topic: &'a str,
    pub(crate) message: &'a str,
}

enum MqttRequest {
    Send(MqttReply, OneshotSender<Result<()>>),
    SendRetained(MqttReply, OneshotSender<Result<()>>),
    HangUp(OneshotSender<()>),
    Subscribe(String, OneshotSender<Result<MqttInstance>>),
    LastWill {
        topic: String,
        message: String,
        reply: OneshotSender<Result<LastWillMqtt>>,
    },
}

pub(crate) struct LastWillMqtt {
    cancel: CancellationToken,
}

impl LastWillMqtt {
    pub(crate) async fn new(
        config: &MqttServerConfig,
        topic: String,
        message: String,
    ) -> AnyResult<Self> {
        log::trace!("Run MQTT Last Will");
        let mut mqttoptions = MqttOptions::new(
            format!("NeolinkLastWill_{}_{}", topic, Uuid::new_v4()),
            &config.broker_addr,
            config.port,
        );
        let max_size = 100 * (1024 * 1024);
        mqttoptions.set_max_packet_size(max_size, max_size);

        // Use TLS if ca path is set
        if let Some(ca_path) = &config.ca {
            if let Ok(ca) = std::fs::read(ca_path) {
                // Use client_auth if they have cert and key
                let client_auth = if let Some((cert_path, key_path)) = &config.client_auth {
                    if let (Ok(cert_buf), Ok(key_buf)) =
                        (std::fs::read(cert_path), std::fs::read(key_path))
                    {
                        Some((cert_buf, key_buf))
                    } else {
                        error!("Failed to set client tls");
                        None
                    }
                } else {
                    None
                };

                let transport = Transport::Tls(TlsConfiguration::Simple {
                    ca,
                    alpn: None,
                    client_auth,
                });
                mqttoptions.set_transport(transport);
            } else {
                error!("Failed to set CA");
            }
        };

        if let Some((username, password)) = &config.credentials {
            mqttoptions.set_credentials(username, password);
        }

        mqttoptions.set_keep_alive(Duration::from_secs(5));

        // On unclean disconnect send this
        mqttoptions.set_last_will(LastWill::new(topic, message, QoS::AtLeastOnce, true));

        let (client, mut connection) = AsyncClient::new(mqttoptions, 100);
        let client = Arc::new(client);
        let cancel = CancellationToken::new();
        let thread_cancel = cancel.clone();

        tokio::task::spawn(async move {
            loop {
                let r = tokio::select! {
                    _ = thread_cancel.cancelled() => AnyResult::Ok(()),
                    v = connection.poll() =>  {
                        v?;
                        AnyResult::Ok(())
                    },
                };
                if r.is_ok() {
                    continue;
                }
                break r;
            }?;
            drop(client);
            AnyResult::Ok(())
        });

        Ok(LastWillMqtt { cancel })
    }
}

impl Drop for LastWillMqtt {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        topic: &str,
    ) -> (
        MqttReply,
        tokio::sync::oneshot::Receiver<Result<()>>,
        OneshotSender<Result<()>>,
    ) {
        let (tx, rx) = oneshot();
        (
            MqttReply {
                topic: topic.to_string(),
                message: Arc::new("payload".to_string()),
            },
            rx,
            tx,
        )
    }

    #[tokio::test]
    async fn publish_request_acknowledges_the_sender() {
        let (client, _eventloop) =
            AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 10);
        let (msg, rx, tx) = request("neolink/status");
        let mut pending = Some(MqttRequest::Send(msg, tx));

        publish_request(&client, &mut pending).await;

        assert!(pending.is_none(), "an accepted request is not requeued");
        assert!(rx.await.expect("reply channel kept alive").is_ok());
    }

    /// The reason the daemon used to die whenever the broker was refusing
    /// connections: a request in flight when the backend went away took its
    /// reply channel with it, and the caller read that as a hard failure.
    #[tokio::test]
    async fn publish_request_keeps_the_request_when_the_connection_is_gone() {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 10);
        drop(eventloop);
        let (msg, mut rx, tx) = request("neolink/status");
        let mut pending = Some(MqttRequest::SendRetained(msg, tx));

        publish_request(&client, &mut pending).await;

        assert!(
            matches!(pending, Some(MqttRequest::SendRetained(..))),
            "a refused request stays intact so it can be requeued"
        );
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "the sender is left waiting rather than failed"
        );
    }
}
