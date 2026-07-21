//!
//! # Neolink MQTT
//!
//! Handles incoming and outgoing MQTT messages
//!
//! This acts as a bridge between cameras and MQTT servers
//!
//! Messages are prefixed with `neolink/{CAMERANAME}`
//!
//! Control messages:
//!
//! - `/control/floodlight [on|off]` Turns floodlight (if equipped) on/off
//! - `/control/led [on|off]` Turns status LED on/off
//! - `/control/pir [on|off]` Turns PIR on/off
//! - `/control/ir [on|off|auto]` Turn IR lights on/off or automatically via light detection
//! - `/control/reboot` Reboot the camera
//! - `/control/ptz` [up|down|left|right|in|out] (amount) Control the PTZ movements, amount defaults to 32.0
//! - `/control/ptz/preset` [id] Move the camera to a known preset
//! - `/control/ptz/assign` [id] [name] Assign the current ptz position to an ID and name
//! - `/control/privacy` [on|off] Toggle the Baichuan privacy-mode shutter
//! - `/control/scene` [off|<id>] Activate a host scene (arming scenario)
//!   by id, or disable scene mode. Conventional ids: 0/off = disable,
//!   1 = away, 2 = home, 3 = disarm.
//!
//! Status Messages:
//!
//! `/status offline` Sent when the neolink goes offline this is a LastWill message
//! `/status disconnected` Sent when the camera goes offline
//! `/status/battery` Sent in reply to a `/query/battery`
//! `/status/pir` Sent in reply to a `/query/pir`
//! `/status/ptz/preset` Sent in reply to a `/query/ptz/preset`
//! `/status/privacy` Current Baichuan privacy-mode state (on|off).
//!   Pushed by the camera on change and also on `/query/privacy`.
//! `/status/scene` Last scene id activated via `/control/scene`
//!   (or "off" when scene mode is disabled).
//! `/status/scene_list` Comma-separated list of available scene ids.
//!
//! Query Messages:
//!
//! `/query/battery` Request that the camera reports its battery level
//! `/query/pir` Request that the camera reports its pir status
//! `/query/ptz/preset` Request that the camera reports the PTZ presets
//! `/query/preview` Request that the camera post a base64 encoded jpeg
//!    of the stream to `/status/preview`
//! `/query/privacy` Request that the camera reports its current
//!    privacy-mode state on `/status/privacy`
//! `/query/scene` Request the available scene ids on `/status/scene_list`
//!
//!
//! # Usage
//!
//! ```bash
//! neolink mqtt --config=config.toml
//! ```
//!
//! # Example Config
//!
//! ```toml
//! [[cameras]]
//! name = "Cammy"
//! username = "****"
//! password = "****"
//! address = "****:9000"
//!   [cameras.mqtt]
//!   server = "127.0.0.1"
//!   port = 1883
//!   credentials = ["username", "password"]
//! ```
//!
//! `server` is the mqtt server
//! `port` is the mqtt server's port
//! `credentials` are the username and password required to identify with the mqtt server
//!
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use std::collections::{HashMap, HashSet};
use tokio::{
    sync::mpsc::channel as mpsc,
    task::JoinSet,
    time::{interval, sleep, Duration, MissedTickBehavior},
};
use tokio_stream::{wrappers::IntervalStream, StreamExt};
use tokio_util::sync::CancellationToken;
use validator::Validate;

use neolink_core::bc_protocol::{Direction as BcDirection, LightState};

mod cmdline;
mod discovery;
mod mqttc;

use crate::{
    common::{MdState, NeoInstance, NeoReactor},
    config::Config,
    AnyResult,
};
use anyhow::{anyhow, Context, Result};
pub(crate) use cmdline::Opt;
pub(crate) use discovery::Discoveries;
use log::*;
use mqttc::{Mqtt, MqttReplyRef};

use self::{
    discovery::enable_discovery,
    mqttc::{MqttInstance, MqttReply},
};

/// Entry point for the mqtt subcommand
///
/// Opt is the command line options
pub(crate) async fn main(_: Opt, reactor: NeoReactor) -> Result<()> {
    let mut set = tokio::task::JoinSet::new();
    let global_cancel = CancellationToken::new();
    let cancel_drop = global_cancel.clone().drop_guard();
    let config = reactor.config().await?;
    let mqtt = Mqtt::new(config.clone()).await;

    // Startup and stop cameras as they are added/removed to the config
    let thread_cancel = global_cancel.clone();
    let mut thread_config = config.clone();
    let thread_reactor = reactor.clone();
    let thread_instance = mqtt.subscribe("").await?;
    set.spawn(async move {
        let mut set = JoinSet::<AnyResult<()>>::new();
        let thread_cancel2 = thread_cancel.clone();
        tokio::select!{
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                let mut cameras: HashMap<String, CancellationToken> = Default::default();
                let mut config_names = HashSet::new();
                loop {
                    thread_config.wait_for(|config| {
                        let current_names = config.cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();
                        current_names != config_names
                    }).await.with_context(|| "Camera Config Watcher")?;
                    config_names = thread_config.borrow().clone().cameras.iter().filter(|a| a.enabled).map(|cam_config| cam_config.name.clone()).collect::<HashSet<_>>();

                    for name in config_names.iter() {
                        log::info!("{name}: MQTT Starting");
                        if ! cameras.contains_key(name) {
                            let local_cancel = CancellationToken::new();
                            cameras.insert(name.clone(),local_cancel.clone());

                            let thread_global_cancel = thread_cancel2.clone();
                            let thread_reactor2 = thread_reactor.clone();
                            let mqtt_instance = thread_instance.subscribe(name).await?;
                            let name = name.clone();
                            set.spawn(async move {
                                loop {
                                    let camera = thread_reactor2.get(&name).await?;
                                    let mqtt_instance = mqtt_instance.resubscribe().await?;
                                    let r = tokio::select!{
                                        _ = thread_global_cancel.cancelled() => {
                                            AnyResult::Ok(())
                                        },
                                        _ = local_cancel.cancelled() => {
                                            AnyResult::Ok(())
                                        },
                                        v = listen_on_camera(camera, mqtt_instance) => {
                                            v
                                        },
                                    };
                                    if let Ok(()) = &r {
                                        break r
                                    } else {
                                        continue;
                                    }
                                }
                            }) ;
                        }
                    }

                    for (running_name, token) in cameras.iter() {
                        if ! config_names.contains(running_name) {
                            token.cancel();
                        }
                    }
                }
            } => v,
        }
    });

    // This threads prints the config
    let mut thread_config = config.clone();
    let thread_instance = mqtt.subscribe("").await?;
    let thread_cancel = global_cancel.clone();
    set.spawn(async move {
        tokio::select! {
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                let mut curr_config = thread_config.borrow().clone();
                let str = toml::to_string(&curr_config)?;
                thread_instance.send_message("config", &str, true).await?;
                loop {
                    curr_config = thread_config
                        .wait_for(|new_conf| new_conf != &curr_config)
                        .await?
                        .clone();
                    let str = toml::to_string(&curr_config)?;
                    thread_instance.send_message("config", &str, true).await?;
                    log::trace!("UpdatedPosted config");
                }
            } => v,
        }
    });

    // This threads checks for config changes on the mqtt
    let thread_config = config.clone();
    let mut thread_instance = mqtt.subscribe("").await?;
    let thread_reactor = reactor.clone();
    let thread_cancel = global_cancel.clone();
    set.spawn(async move {
        tokio::select! {
            _ = thread_cancel.cancelled() => AnyResult::Ok(()),
            v = async {
                while let Ok(msg) = thread_instance.recv().await {
                    if msg.topic == "config" {
                        let config: Result<Config> = toml::from_str(&msg.message).with_context(|| {
                            format!("Failed to parse the MQTT {:?} config file", msg.topic)
                        });
                        if let Err(e) = config {
                            thread_instance
                                .send_message("config/status", &format!("{:?}", e), false)
                                .await?;
                            continue;
                        }
                        let curr_config = thread_config.borrow().clone();
                        let mut config = config?;

                        // Fill in skipped passwords
                        if let (Some(mqtt), Some(curr_mqtt)) = (config.mqtt.as_mut(), curr_config.mqtt.as_ref()) {
                            if mqtt.credentials.is_none() {
                                mqtt.credentials = curr_mqtt.credentials.clone();
                            }
                            if mqtt.ca.is_none() {
                                mqtt.ca = curr_mqtt.ca.clone();
                            }
                            if mqtt.client_auth.is_none() {
                                mqtt.client_auth = curr_mqtt.client_auth.clone();
                            }
                        }
                        for cam in config.cameras.iter_mut() {
                            let name = cam.name.clone();
                            let cur_cam = curr_config.cameras.iter().find(|c| c.name == name);
                            if let Some(cur_cam) = cur_cam.as_ref() {
                                if cam.password.is_none() {
                                    cam.password = cur_cam.password.clone();
                                }
                            }
                        }
                        for user in config.users.iter_mut() {
                            let name = user.name.clone();
                            let cur_user = curr_config.users.iter().find(|c| c.name == name);
                            if let Some(cur_user) = cur_user.as_ref() {
                                if user.pass.is_none() {
                                    user.pass = cur_user.pass.clone();
                                }
                            }
                        }
                        // Passwords should now be restored if they were not set

                        let validate = config.validate().with_context(|| {
                            format!("Failed to validate the MQTT {:?} config file", msg.topic)
                        });
                        if let Err(e) = validate {
                            thread_instance
                                .send_message("config/status", &format!("{:?}", e), false)
                                .await?;
                            continue;
                        }

                        if (*thread_config.borrow()) == config {
                            continue;
                        }

                        let result = thread_reactor.update_config(config).await;
                        thread_instance
                            .send_message("config/status", &format!("{:?}", result), false)
                            .await?;
                        log::info!("Updated config");
                    }
                }
                AnyResult::Ok(())
            } => v,
        }
    });

    while let Some(result) = set.join_next().await {
        if let Err(_) | Ok(Err(_)) = &result {
            global_cancel.cancel();
            result??;
        }
    }

    drop(cancel_drop);
    Ok(())
}

async fn listen_on_camera(camera: NeoInstance, mqtt_instance: MqttInstance) -> Result<()> {
    let mut watch_config = camera.config().await?;
    let camera_name = watch_config.borrow().name.clone();
    let mut config;
    let cancel = CancellationToken::new();
    let drop_cancel = cancel.clone().drop_guard();
    let r = loop {
        config = watch_config.borrow().clone().mqtt;
        break tokio::select! {
            v = watch_config.wait_for(|new_config| config != new_config.mqtt) => {
                v?;
                continue;
            }
            v = async {
                //Publish initial states
                mqtt_instance
                        .send_message("status", "disconnected", true)
                        .await
                        .with_context(|| format!("Failed to publish status for {}", camera_name))?;
                let _drop_message = mqtt_instance.last_will("status", "disconnected").await?;
                mqtt_instance
                    .send_message("status/motion", "unknown", true)
                    .await
                    .with_context(|| format!("Failed to publish motion unknown for {}", camera_name))?;
                mqtt_instance
                    .send_message("status/notification", "unknown", true)
                    .await
                    .with_context(|| format!("Failed to publish push notification unknown for {}", camera_name))?;
                let _drop_message2 = mqtt_instance.last_will("status/motion", "unknown").await?;

                if let Some(discovery_config) = config.discovery.as_ref() {
                    enable_discovery(discovery_config, &mqtt_instance, &camera).await?;
                }

                let camera_msg = camera.clone();
                let mut mqtt_msg = mqtt_instance.resubscribe().await?;
                let cancel_msg = cancel.clone();
                let mut set_msg = JoinSet::new();

                let mut camera_watch = camera.camera();
                let mqtt_watch = mqtt_instance.resubscribe().await?;

                let camera_floodlight = camera.clone();
                let mqtt_floodlight = mqtt_instance.resubscribe().await?;

                let camera_motion = camera.clone();
                let mqtt_motion = mqtt_instance.resubscribe().await?;

                #[cfg(feature = "pushnoti")]
                let camera_pn = camera.clone();
                #[cfg(feature = "pushnoti")]
                let mqtt_pn = mqtt_instance.resubscribe().await?;

                let camera_snap = camera.clone();
                let mqtt_snap = mqtt_instance.resubscribe().await?;

                let camera_battery = camera.clone();
                let mqtt_battery = mqtt_instance.resubscribe().await?;

                let camera_floodlight_tasks = camera.clone();
                let mqtt_floodlight_tasks = mqtt_instance.resubscribe().await?;

                let camera_privacy = camera.clone();
                let mqtt_privacy = mqtt_instance.resubscribe().await?;

                let camera_scene = camera.clone();
                let mqtt_scene = mqtt_instance.resubscribe().await?;

                // Snapshot the per-feature flags + poll intervals up front so
                // the &config borrow used by the select! guard expressions
                // does not conflict with by-move captures inside the async
                // branches below.
                let enable_privacy = config.enable_privacy;
                let privacy_update = config.privacy_update;
                let enable_scene = config.enable_scene;
                let scene_update = config.scene_update;

                tokio::select! {
                    _ = cancel.cancelled() => AnyResult::Ok(()),
                    // Handles incomming requests
                    v  = async {
                        let (tx, mut rx) = mpsc(1);
                        tokio::select! {
                            v = async {
                                log::debug!("Listening to message on {}", mqtt_msg.get_name());
                                while let Ok(msg) = mqtt_msg.recv().await {
                                    let mqtt_msg = mqtt_msg.resubscribe().await?;
                                    let camera_msg = camera_msg.clone();
                                    let tx = tx.clone();
                                    let cancel_msg = cancel_msg.clone();
                                    set_msg.spawn(async move {
                                        tokio::select!{
                                            _ = cancel_msg.cancelled() => AnyResult::Ok(()),
                                            v = async {
                                                let res = handle_mqtt_message(msg, &mqtt_msg, &camera_msg).await;
                                                if res.is_err() {
                                                    tx.send(res).await?;
                                                }
                                                AnyResult::Ok(())
                                            } => v,
                                        }
                                    });
                                }
                                AnyResult::Ok(())
                            } => {
                                v
                            },
                            v = rx.recv() => {
                                v.ok_or(anyhow!("All error senders were dropped"))?
                            },
                        }?;
                        AnyResult::Ok(())
                    } => v,
                    // Handle camera disconnect/connect
                    v = async {
                        loop {
                            camera_watch.wait_for(|cam| cam.upgrade().is_some()).await.with_context(|| {
                                format!("{}: Online Watch Dropped", camera_name)
                            })?;
                            log::trace!("Publish online");
                            mqtt_watch.send_message("status", "connected", true).await.with_context(|| {
                                format!("{}: Failed to publish connected", camera_name)
                            })?;
                            camera_watch.wait_for(|cam| cam.upgrade().is_none()).await.with_context(|| {
                                format!("{}: Disconnect Watch Dropped", camera_name)
                            })?;
                            mqtt_watch.send_message("status", "disconnected", true).await.with_context(|| {
                                format!("{}: Failed to publish disconnected", camera_name)
                            })?;
                        }
                    } => {
                        v
                    },
                    // Handle the floodlight
                    v = async {
                        let (tx, mut rx) = mpsc(100);
                        let v = tokio::select! {
                            v = async {
                                loop {
                                    let r = camera_floodlight.run_passive_task(|cam| {
                                        let tx = tx.clone();
                                        Box::pin(
                                            async move {
                                                let mut reciever = tokio_stream::wrappers::ReceiverStream::new(cam.listen_on_flightlight().await?);
                                                while let Some(flights) = reciever.next().await {
                                                    for flight in flights.floodlight_status_list.iter() {
                                                        if flight.status == 0 {
                                                            tx.send(false).await?;
                                                        } else {
                                                            tx.send(true).await?;
                                                        }
                                                    }
                                                }
                                                AnyResult::Ok(())
                                            }
                                        )
                                    }).await;
                                    if r.is_err() {
                                        break r;
                                    }
                                }
                            } => v,
                            v = async {
                                while let Some(on) = rx.recv().await {
                                    if on {
                                        mqtt_floodlight.send_message("status/floodlight", "on", true).await?;
                                    } else {
                                        mqtt_floodlight.send_message("status/floodlight", "off", true).await?;
                                    }
                                }
                                AnyResult::Ok(())
                            } => v,
                        };
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_light => v,
                    // Handle the motion messages
                    v = async {
                        let mut md = camera_motion.motion().await?;
                        loop {
                            let v = async {
                                md.wait_for(|state| matches!(state, MdState::Start(_))).await.with_context(|| {
                                    format!("{}: MdStart Watch Dropped", camera_name)
                                })?;
                                mqtt_motion.send_message("status/motion", "on", true).await.with_context(|| {
                                    format!("{}: Failed to publish motion start", camera_name)
                                })?;
                                md.wait_for(|state| matches!(state, MdState::Stop(_))).await.with_context(|| {
                                    format!("{}: MdStop Watch Dropped", camera_name)
                                })?;
                                mqtt_motion.send_message("status/motion", "off", true).await.with_context(|| {
                                    format!("{}: Failed to publish motion stop", camera_name)
                                })?;
                                AnyResult::Ok(())
                            }.await;
                            match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                                Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                                Ok(()) => AnyResult::Ok(()),
                                Err(Ok(e)) => Err(e.into()),
                                Err(Err(e)) => Err(e),
                            }?;
                        }
                    }, if config.enable_motion => v,
                    // Handle the SNAP (image preview)
                    v = async {
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.preview_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        let v = async {
                            while wait.next().await.is_some() {
                                let image = camera_snap.run_passive_task(|cam| {
                                    Box::pin(async move {
                                        let image = cam.get_snapshot().await?;
                                        AnyResult::Ok(image)
                                    })
                                }).await;
                                let image = match image {
                                    Err(e) => match e.downcast::<neolink_core::Error>() {
                                        Ok(neolink_core::Error::CameraServiceUnavailable{..}) => {
                                            log::debug!("Image not supported");
                                            futures::future::pending().await
                                        },
                                        Ok(e) => Err(e.into()),
                                        Err(e) => Err(e),
                                    }
                                    n => n,
                                }?;
                                mqtt_snap
                                        .send_message("status/preview", BASE64.encode(image).as_str(), true)
                                        .await
                                        .with_context(|| {
                                            format!("{}: Failed to publish preview", camera_name)
                                        })?;
                            }
                            AnyResult::Ok(())
                        }.await;
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_preview => v,
                    // Handle the battery publish
                    v = async {
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.battery_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });

                        let v = async {
                            while wait.next().await.is_some() {
                                let xml = camera_battery.run_passive_task(|cam| {
                                    Box::pin(async move {
                                        let xml = cam.battery_info().await?;
                                        AnyResult::Ok(xml)
                                    })
                                }).await;
                                let xml = match xml {
                                    Err(e) => match e.downcast::<neolink_core::Error>() {
                                        Ok(neolink_core::Error::CameraServiceUnavailable{..}) => {
                                            log::debug!("Battery not supported");
                                            futures::future::pending().await
                                        },
                                        Ok(e) => Err(e.into()),
                                        Err(e) => Err(e),
                                    }
                                    n => n,
                                }?;
                                mqtt_battery
                                        .send_message("status/battery_level", format!("{}", xml.battery_percent).as_str(), true)
                                        .await
                                        .with_context(|| {
                                            format!("{}: Failed to publish battery", camera_name)
                                        })?;
                            }
                            AnyResult::Ok(())
                        }.await;
                        match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                            Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                            Ok(()) => AnyResult::Ok(()),
                            Err(Ok(e)) => Err(e.into()),
                            Err(Err(e)) => Err(e),
                        }?;
                        AnyResult::Ok(())
                    }, if config.enable_battery => v,
                    // Handle the push notification messages
                    v = async {
                        #[cfg(feature = "pushnoti")]
                        {
                            let mut pn = camera_pn.push_notifications().await?;
                            let mut prev_noti = None;
                            loop {
                                let v = async {
                                    let noti = pn.wait_for(|noti| noti != &prev_noti && noti.is_some()).await.with_context(|| {
                                        format!("{}: PushNoti Watch Dropped", camera_name)
                                    })?.clone();
                                    mqtt_pn.send_message("status/notification", &noti.as_ref().unwrap().message, true).await.with_context(|| {
                                        format!("{}: Failed to publish push notification", camera_name)
                                    })?;
                                    prev_noti = noti;
                                    AnyResult::Ok(())
                                }.await;
                                match v.map_err(|e| e.downcast::<neolink_core::Error>()) {
                                    Err(Ok(neolink_core::Error::UnintelligibleReply{..})) => futures::future::pending().await,
                                    Ok(()) => AnyResult::Ok(()),
                                    Err(Ok(e)) => Err(e.into()),
                                    Err(Err(e)) => Err (e),
                                }?;
                            }
                        }
                        #[cfg(not(feature = "pushnoti"))]
                        unreachable!()
                    }, if cfg!(feature = "pushnoti") => v,
                    // Handle the floodlight task activation
                    v = async {
                        let flt_status = camera_floodlight_tasks.run_passive_task(|cam| Box::pin(async move {
                            Ok(cam.is_flightlight_tasks_enabled().await?)
                        })).await;
                        if flt_status.is_err() {
                            // Assume floodlight unsupported
                            futures::future::pending::<()>().await;
                        }
                        let flt_status = flt_status.unwrap();
                        let flt_status_txt = match flt_status {
                            true => "on".to_string(),
                            false => "off".to_string(),
                        };
                        mqtt_floodlight_tasks.send_message("status/floodlight_tasks", &flt_status_txt, true).await.with_context(|| {
                            format!("{}: Failed to publish floodlight task notification", camera_name)
                        })?;

                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(config.floodlight_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        while wait.next().await.is_some() {
                            let flt_status = camera_floodlight_tasks.run_passive_task(|cam| Box::pin(async move {
                                Ok(cam.is_flightlight_tasks_enabled().await?)
                            })).await;
                            if let Ok(flt_status) = flt_status {
                                let flt_status_txt = match flt_status {
                                    true => "on".to_string(),
                                    false => "off".to_string(),
                                };
                                mqtt_floodlight_tasks.send_message("status/floodlight_tasks", &flt_status_txt, true).await.with_context(|| {
                                    format!("{}: Failed to publish floodlight task notification", camera_name)
                                })?;
                            }
                        }
                        AnyResult::Ok(())
                    }, if config.enable_floodlight => v,
                    // Handle the privacy-mode state (cmd 622 / 623).
                    //
                    // The cmd-623 push handler is registered on the camera
                    // connection and forwarded over an mpsc channel; a
                    // periodic poll primes the value at startup and acts as
                    // a fallback for firmwares that don't push reliably.
                    v = async {
                        let initial = camera_privacy.run_passive_task(|cam| Box::pin(async move {
                            Ok(cam.get_privacy_mode().await?)
                        })).await;
                        if initial.is_err() {
                            // Assume privacy mode unsupported on this camera
                            log::debug!("{}: Privacy mode unsupported, skipping", camera_name);
                            futures::future::pending::<()>().await;
                        }
                        let on = initial.unwrap();
                        let txt = if on { "on" } else { "off" };
                        mqtt_privacy.send_message("status/privacy", txt, true).await.with_context(|| {
                            format!("{}: Failed to publish privacy state", camera_name)
                        })?;
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(privacy_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        while wait.next().await.is_some() {
                            if let Ok(on) = camera_privacy.run_passive_task(|cam| Box::pin(async move {
                                Ok(cam.get_privacy_mode().await?)
                            })).await {
                                let txt = if on { "on" } else { "off" };
                                mqtt_privacy.send_message("status/privacy", txt, true).await.with_context(|| {
                                    format!("{}: Failed to publish privacy state", camera_name)
                                })?;
                            }
                        }
                        AnyResult::Ok(())
                    }, if enable_privacy => v,
                    // Handle the scene-mode state (cmd 603 / 605).
                    //
                    // We poll the scene-list periodically. There is no
                    // separately addressable "currently active scene id"
                    // command in the public spec; status/scene_list
                    // reports the configured ids and status/scene tracks
                    // the last value we set through control/scene.
                    v = async {
                        let initial = camera_scene.run_passive_task(|cam| Box::pin(async move {
                            Ok(cam.get_scenes().await?)
                        })).await;
                        if initial.is_err() {
                            log::debug!("{}: Scene mode unsupported, skipping", camera_name);
                            futures::future::pending::<()>().await;
                        }
                        let format_csv = |ids: &[u8]| {
                            ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
                        };
                        mqtt_scene.send_message("status/scene_list", &format_csv(&initial.unwrap()), true).await.with_context(|| {
                            format!("{}: Failed to publish scene list", camera_name)
                        })?;
                        let mut wait = IntervalStream::new({
                            let mut i = interval(Duration::from_millis(scene_update));
                            i.set_missed_tick_behavior(MissedTickBehavior::Skip);
                            i
                        });
                        while wait.next().await.is_some() {
                            if let Ok(ids) = camera_scene.run_passive_task(|cam| Box::pin(async move {
                                Ok(cam.get_scenes().await?)
                            })).await {
                                mqtt_scene.send_message("status/scene_list", &format_csv(&ids), true).await.with_context(|| {
                                    format!("{}: Failed to publish scene list", camera_name)
                                })?;
                            }
                        }
                        AnyResult::Ok(())
                    }, if enable_scene => v,
                }?;
                AnyResult::Ok(())
            } => v,
        };
    };

    drop(drop_cancel);
    r?;
    Ok(())
}

async fn handle_mqtt_message(
    msg: MqttReply,
    mqtt: &MqttInstance,
    camera: &NeoInstance,
) -> Result<()> {
    match msg.as_ref() {
        MqttReplyRef {
            topic: _,
            message: "OK",
        }
        | MqttReplyRef {
            topic: _,
            message: "FAIL",
        } => {
            // Do nothing for the success/fail replies
        }
        MqttReplyRef { topic: _, message }
            if message.starts_with("FAIL:") || message.starts_with("OK:") =>
        {
            // Do nothing for the success/fail replies
        }
        MqttReplyRef {
            topic: "control/floodlight",
            message: "on",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.set_floodlight_manual(true, 180).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the floodlight light: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/floodlight", &reply, false)
                .await
                .with_context(|| "Failed to publish camera status light on")?;
        }
        MqttReplyRef {
            topic: "control/floodlight",
            message: "off",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.set_floodlight_manual(false, 180).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the floodlight light: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/floodlight", &reply, false)
                .await
                .with_context(|| "Failed to publish camera status light off")?;
        }
        MqttReplyRef {
            topic: "control/led",
            message: "on",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.led_light_set(true).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/led", &reply, false)
                .await
                .with_context(|| "Failed to publish led on")?;
        }
        MqttReplyRef {
            topic: "control/led",
            message: "off",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.led_light_set(false).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/led", &reply, false)
                .await
                .with_context(|| "Failed to publish led off")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "on",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::On).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the ir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir on")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "off",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::Off).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the ir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir off")?;
        }
        MqttReplyRef {
            topic: "control/ir",
            message: "auto",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.irled_light_set(LightState::Auto).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn set to auto on the led: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ir auto")?;
        }
        MqttReplyRef {
            topic: "control/reboot",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.reboot().await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to reboot the camera: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish reboot on the camera")?;
        }
        MqttReplyRef {
            topic: "control/zoom",
            message,
        } => {
            let reply = if let Ok(amount) = message.parse::<f32>() {
                if let Err(e) = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.zoom_to((amount * 1000.0) as u32).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await
                {
                    error!("Failed to send PTZ: {:?}", e);
                    format!("FAIL: {e:?}")
                } else {
                    "OK".to_string()
                }
            } else {
                "FAIL: Could not convert message to number".to_string()
            };

            mqtt.send_message("control/zoom", &reply, false)
                .await
                .with_context(|| "Failed to publish zoom on the camera")?;
        }
        MqttReplyRef {
            topic: "control/ptz",
            message,
        }
        | MqttReplyRef {
            topic: "control/pt",
            message,
        } => {
            let lowercase_message = message.to_lowercase();
            let mut words = lowercase_message.split_whitespace();
            let reply = if let Some(direction_txt) = words.next() {
                // Target amount to move
                let speed = 32f32;
                let amount = words.next().unwrap_or("32.0");

                if let Ok(amount) = amount.parse::<f32>() {
                    let seconds = amount / speed;
                    // range checking on seconds so that you can't sleep for 3.4E+38 seconds
                    let seconds = match seconds {
                        x if (0.0..10.0).contains(&x) => Some(seconds),
                        _ => {
                            error!("seconds was not a valid number (out of range)");
                            None
                        }
                    };

                    let bc_direction = match direction_txt {
                        "up" => Some(BcDirection::Up),
                        "down" => Some(BcDirection::Down),
                        "left" => Some(BcDirection::Left),
                        "right" => Some(BcDirection::Right),
                        n => {
                            error!("Unrecognized PTZ direction \"{}\"", n);
                            None
                        }
                    };

                    if let (Some(seconds), Some(bc_direction)) = (seconds, bc_direction) {
                        // On drop send the stop command again just to make sure it stops
                        let _drop_command = camera.clone().drop_command(
                            move |cam| {
                                Box::pin(async move {
                                    cam.send_ptz(BcDirection::Stop, speed).await?;
                                    AnyResult::Ok(())
                                })
                            },
                            Duration::from_millis(100),
                        );
                        if let Err(e) = camera
                            .run_task(|cam| {
                                Box::pin(async move {
                                    cam.send_ptz(bc_direction, speed).await?;
                                    sleep(Duration::from_secs_f32(seconds)).await;
                                    cam.send_ptz(BcDirection::Stop, speed).await?;
                                    AnyResult::Ok(())
                                })
                            })
                            .await
                        {
                            error!("Failed to send PTZ: {:?}", e);
                            "FAIL"
                        } else {
                            "OK"
                        }
                    } else {
                        "FAIL"
                    }
                } else {
                    error!("No PTZ speed as a valid number");
                    "FAIL"
                }
            } else {
                error!("No PTZ Direction given. Please add up/down/left/right/in/out");
                "FAIL"
            }
            .to_string();

            mqtt.send_message("control/ptz", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz on the camera")?;
        }
        MqttReplyRef {
            topic: "control/ptz/preset",
            message,
        } => {
            let reply = if let Ok(id) = message.parse::<u8>() {
                let res = camera
                    .run_task(|cam| {
                        Box::pin(async move {
                            cam.moveto_ptz_preset(id).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await;
                if res.is_err() {
                    error!("Failed to move to ptz preset: {:?}", res.err());
                    "FAIL"
                } else {
                    "OK"
                }
            } else {
                error!("PTZ preset was not a valid number");
                "FAIL"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz move")?;
        }
        MqttReplyRef {
            topic: "control/ptz/assign",
            message,
        } => {
            let mut words = message.split_whitespace();
            let id = words.next();
            let name = words.next();

            let reply = if let (Some(Ok(id)), Some(name)) = (id.map(|id| id.parse::<u8>()), name) {
                let name = name.to_owned();
                let res = camera
                    .run_task(|cam| {
                        let name = name.clone();
                        Box::pin(async move {
                            cam.set_ptz_preset(id, name).await?;
                            AnyResult::Ok(())
                        })
                    })
                    .await;
                if res.is_err() {
                    error!("Failed to assign ptz preset: {:?}", res.err());
                    "FAIL"
                } else {
                    "OK"
                }
            } else if let (Some(Err(_)), _) = (id.map(|id| id.parse::<u8>()), name) {
                error!("PTZ preset was not a valid number");
                "FAIL"
            } else if let (_, None) = (id.map(|id| id.parse::<u8>()), name) {
                error!("PTZ preset was not given a name");
                "FAIL"
            } else {
                "FAIL"
            }
            .to_string();
            mqtt.send_message("control/ir", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz move")?;
        }
        MqttReplyRef {
            topic: "control/pir",
            message: "on",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.pir_set(true).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn on the pir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir on")?;
        }
        MqttReplyRef {
            topic: "control/pir",
            message: "off",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.pir_set(false).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if res.is_err() {
                error!("Failed to turn off the pir: {:?}", res.err());
                "FAIL"
            } else {
                "OK"
            }
            .to_string();
            mqtt.send_message("control/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir off")?;
        }
        MqttReplyRef {
            topic: "control/wakeup",
            message,
        } => {
            let reply = match message.parse::<u64>() {
                Ok(secs) => {
                    if let Ok(permit) = camera.permit().await {
                        // This task waits for the `run_task` to send the OK then starts the countdown
                        // to drop the permit
                        let camera = camera.clone();
                        tokio::task::spawn(async move {
                            // Wait for connection then start the countdown
                            // By using a run_task we can delay the countdown until AFTER we are connected
                            let _ = camera
                                .run_task(|_cam| Box::pin(async move { AnyResult::Ok(()) }))
                                .await;

                            sleep(Duration::from_secs(secs * 60)).await;

                            drop(permit);
                        });
                        "OK"
                    } else {
                        "FAIL: Camera shutting down"
                    }
                    .to_string()
                }
                Err(e) => {
                    error!("Failed to parse minutes: {:?}", e);
                    format!("FAIL: '{message}' => {e:?}")
                }
            };

            mqtt.send_message("control/wakeup", &reply, false)
                .await
                .with_context(|| "Failed to publish wakeup")?;
        }
        MqttReplyRef {
            topic: "control/floodlight_tasks",
            message,
        } => {
            let state = match message.to_lowercase().as_ref() {
                "on" => Ok(true),
                "off" => Ok(false),
                n => match n.parse::<bool>() {
                    Ok(state) => Ok(state),
                    Err(e) => AnyResult::Err(e.into()),
                },
            };

            let reply = match state {
                Ok(state) => {
                    if let Err(e) = camera
                        .run_task(|cam| {
                            Box::pin(async move {
                                cam.flightlight_tasks_enable(state).await?;
                                AnyResult::Ok(())
                            })
                        })
                        .await
                    {
                        format!("FAIL: {e:?}")
                    } else {
                        "OK".to_string()
                    }
                }
                Err(e) => format!("FAIL: Could not parse message to {e:?}"),
            };

            mqtt.send_message("control/floodlight_tasks", &reply, false)
                .await
                .with_context(|| "Failed to publish floodlight_tasks")?;
        }
        MqttReplyRef {
            topic: "control/siren",
            message: "on",
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.siren().await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if let Err(e) = res {
                error!("Failed to trigger siren: {:?}", e);
                format!("FAIL: {e:?}")
            } else {
                "OK".to_string()
            };

            mqtt.send_message("control/siren", &reply, false)
                .await
                .with_context(|| "Failed to publish siren")?;
        }
        MqttReplyRef {
            topic: "query/battery",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let xml = cam.battery_info().await?;
                        AnyResult::Ok(xml)
                    })
                })
                .await;
            let reply = match res {
                Err(e) => {
                    error!("Failed to get battery xml: {:?}", e);
                    "FAIL"
                }
                Ok(xml) => {
                    let ser_xml = {
                        let mut buf = bytes::BytesMut::new();
                        quick_xml::se::to_writer(&mut buf, &xml).map(|_| buf.to_vec())
                    };
                    match ser_xml {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(str) => {
                                mqtt.send_message("status/battery", &str, false)
                                    .await
                                    .with_context(|| "Failed to publish battery info")?;
                                "OK"
                            }
                            Err(_) => {
                                error!("Failed to encode battery status");
                                "FAIL"
                            }
                        },
                        Err(_) => {
                            error!("Failed to serialise battery status");
                            "FAIL"
                        }
                    }
                }
            }
            .to_string();
            mqtt.send_message("query/battery", &reply, false)
                .await
                .with_context(|| "Failed to publish battery query")?;
        }
        MqttReplyRef {
            topic: "query/pir", ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let xml = cam.get_pirstate().await?;
                        AnyResult::Ok(xml)
                    })
                })
                .await;
            let reply = match res {
                Err(e) => {
                    error!("Failed to get pir xml: {:?}", e);
                    "FAIL"
                }
                Ok(xml) => {
                    let ser_xml = {
                        let mut buf = bytes::BytesMut::new();
                        quick_xml::se::to_writer(&mut buf, &xml).map(|_| buf.to_vec())
                    };
                    match ser_xml {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(str) => {
                                mqtt.send_message("status/pir", &str, false)
                                    .await
                                    .with_context(|| "Failed to publish pir info")?;
                                "OK"
                            }
                            Err(_) => {
                                error!("Failed to encode pir status");
                                "FAIL"
                            }
                        },
                        Err(_) => {
                            error!("Failed to serialise pir status");
                            "FAIL"
                        }
                    }
                }
            }
            .to_string();
            mqtt.send_message("query/pir", &reply, false)
                .await
                .with_context(|| "Failed to publish pir query")?;
        }
        MqttReplyRef {
            topic: "query/ptz/preset",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let xml = cam.get_ptz_preset().await?;
                        AnyResult::Ok(xml)
                    })
                })
                .await;
            let reply = match res {
                Err(e) => {
                    error!("Failed to get ptz xml: {:?}", e);
                    "FAIL"
                }
                Ok(xml) => {
                    let ser_xml = {
                        let mut buf = bytes::BytesMut::new();
                        quick_xml::se::to_writer(&mut buf, &xml).map(|_| buf.to_vec())
                    };
                    match ser_xml {
                        Ok(bytes) => match String::from_utf8(bytes) {
                            Ok(str) => {
                                mqtt.send_message("status/ptz", &str, false)
                                    .await
                                    .with_context(|| "Failed to publish ptz info")?;
                                "OK"
                            }
                            Err(_) => {
                                error!("Failed to encode ptz status");
                                "FAIL"
                            }
                        },
                        Err(_) => {
                            error!("Failed to serialise ptz status");
                            "FAIL"
                        }
                    }
                }
            }
            .to_string();
            mqtt.send_message("query/ptz", &reply, false)
                .await
                .with_context(|| "Failed to publish ptz query")?;
        }
        MqttReplyRef {
            topic: "control/privacy",
            message,
        } if message == "on" || message == "off" => {
            let enable = message == "on";
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        cam.set_privacy_mode(enable).await?;
                        AnyResult::Ok(())
                    })
                })
                .await;
            let reply = if let Err(e) = res {
                error!("Failed to set privacy mode: {:?}", e);
                "FAIL"
            } else {
                // Optimistically publish the new state so subscribers see it
                // before the camera pushes its own cmd-623 confirmation.
                let new_state = if enable { "on" } else { "off" };
                let _ = mqtt
                    .send_message("status/privacy", new_state, true)
                    .await
                    .with_context(|| "Failed to publish privacy state");
                "OK"
            }
            .to_string();
            mqtt.send_message("control/privacy", &reply, false)
                .await
                .with_context(|| "Failed to publish privacy ack")?;
        }
        MqttReplyRef {
            topic: "control/scene",
            message,
        } => {
            // Accepted payloads: "off" (disable scene), or a numeric id.
            // "0" is treated as off (matches set_scene's own semantics).
            let parsed: Result<Option<u8>, _> = if message == "off" {
                Ok(None)
            } else {
                message.parse::<u8>().map(Some)
            };
            let res = match parsed {
                Ok(maybe_id) => {
                    camera
                        .run_task(|cam| {
                            Box::pin(async move {
                                match maybe_id {
                                    None | Some(0) => cam.disable_scene().await?,
                                    Some(id) => cam.set_scene(id).await?,
                                }
                                AnyResult::Ok(())
                            })
                        })
                        .await
                }
                Err(ref e) => Err(anyhow!("Invalid scene payload {:?}: {}", message, e)),
            };
            let reply = if let Err(e) = res {
                error!("Failed to set scene: {:?}", e);
                "FAIL"
            } else {
                // Optimistic status publish so listeners reflect the change
                // immediately. Normalised value: "off" or "<id>".
                let normalised = match parsed.as_ref() {
                    Ok(None) | Ok(Some(0)) => "off".to_string(),
                    Ok(Some(id)) => id.to_string(),
                    Err(_) => message.to_string(),
                };
                let _ = mqtt
                    .send_message("status/scene", &normalised, true)
                    .await
                    .with_context(|| "Failed to publish scene state");
                "OK"
            }
            .to_string();
            mqtt.send_message("control/scene", &reply, false)
                .await
                .with_context(|| "Failed to publish scene ack")?;
        }
        MqttReplyRef {
            topic: "query/privacy",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let on = cam.get_privacy_mode().await?;
                        AnyResult::Ok(on)
                    })
                })
                .await;
            let reply = match res {
                Ok(on) => {
                    let txt = if on { "on" } else { "off" };
                    if let Err(e) = mqtt
                        .send_message("status/privacy", txt, true)
                        .await
                        .with_context(|| "Failed to publish privacy")
                    {
                        error!("Failed to send privacy state: {e:?}");
                        "FAIL"
                    } else {
                        "OK"
                    }
                }
                Err(e) => {
                    error!("Failed to read privacy mode: {:?}", e);
                    "FAIL"
                }
            }
            .to_string();
            mqtt.send_message("query/privacy", &reply, false)
                .await
                .with_context(|| "Failed to publish privacy query ack")?;
        }
        MqttReplyRef {
            topic: "query/scene",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let ids = cam.get_scenes().await?;
                        AnyResult::Ok(ids)
                    })
                })
                .await;
            let reply = match res {
                Ok(ids) => {
                    let csv = ids
                        .iter()
                        .map(|i| i.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    if let Err(e) = mqtt
                        .send_message("status/scene_list", &csv, true)
                        .await
                        .with_context(|| "Failed to publish scene list")
                    {
                        error!("Failed to send scene list: {e:?}");
                        "FAIL"
                    } else {
                        "OK"
                    }
                }
                Err(e) => {
                    error!("Failed to read scene list: {:?}", e);
                    "FAIL"
                }
            }
            .to_string();
            mqtt.send_message("query/scene", &reply, false)
                .await
                .with_context(|| "Failed to publish scene query ack")?;
        }
        MqttReplyRef {
            topic: "query/preview",
            ..
        } => {
            let res = camera
                .run_task(|cam| {
                    Box::pin(async move {
                        let data = cam.get_snapshot().await?;
                        AnyResult::Ok(data)
                    })
                })
                .await;
            let reply = match res {
                Err(e) => {
                    error!("Failed to get snapshot: {:?}", e);
                    "FAIL"
                }
                Ok(bytes) => {
                    if let Err(e) = mqtt
                        .send_message("status/preview", BASE64.encode(bytes).as_str(), true)
                        .await
                        .with_context(|| "Failed to publish preview")
                    {
                        error!("Failed to send preview: {e:?}");
                        "FAIL"
                    } else {
                        "OK"
                    }
                }
            }
            .to_string();
            mqtt.send_message("query/preview", &reply, false)
                .await
                .with_context(|| "Failed to publish preview query")?;
        }
        _ => {}
    }
    Ok(())
}
