//! Attempts to subclass GstMediaFactory
//!
//! We are now messing with gstreamer glib objects
//! expect issues

use super::AnyResult;
use crate::config::AudioFormat;
use gstreamer::glib::object_subclass;
use gstreamer::Element;
use gstreamer::{
    glib::{self, Object},
    Structure,
};
use gstreamer_rtsp::RTSPUrl;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::subclass::prelude::*;
use gstreamer_rtsp_server::RTSPMediaFactory;
use gstreamer_rtsp_server::RTSPTransportMode;
use gstreamer_rtsp_server::{RTSP_PERM_MEDIA_FACTORY_ACCESS, RTSP_PERM_MEDIA_FACTORY_CONSTRUCT};
use log::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

glib::wrapper! {
    /// The wrapped RTSPMediaFactory
    pub(crate) struct NeoMediaFactory(ObjectSubclass<NeoMediaFactoryImpl>) @extends RTSPMediaFactory;
}

impl Default for NeoMediaFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl NeoMediaFactory {
    fn new() -> Self {
        let factory = Object::new::<NeoMediaFactory>();
        factory.set_shared(false);
        factory.set_eos_shutdown(false);
        factory.set_stop_on_disconnect(false);
        // factory.set_publish_clock_mode(gstreamer_rtsp_server::RTSPPublishClockMode::Clock);
        factory.set_suspend_mode(gstreamer_rtsp_server::RTSPSuspendMode::Reset);
        factory.set_launch("videotestsrc pattern=\"snow\" ! video/x-raw,width=896,height=512,framerate=25/1 ! textoverlay name=\"inittextoverlay\" text=\"Stream not Ready\" valignment=top halignment=left font-desc=\"Sans, 32\" ! jpegenc ! rtpjpegpay name=pay0");
        factory.set_transport_mode(RTSPTransportMode::PLAY);
        factory
    }

    pub(crate) async fn new_with_callback<F>(callback: F) -> AnyResult<Self>
    where
        F: Fn(Element, Option<AudioFormat>) -> AnyResult<Option<Element>> + Send + Sync + 'static,
    {
        let factory = Self::new();
        factory.imp().set_callback(callback).await;
        Ok(factory)
    }

    pub(crate) fn add_permitted_roles<T: AsRef<str>>(&self, permitted_roles: &HashSet<T>) {
        for permitted_role in permitted_roles {
            let s = permitted_role.as_ref();
            log::debug!("Adding {} as permitted user", s);
            self.add_role_from_structure(
                &Structure::builder(s)
                    .field(RTSP_PERM_MEDIA_FACTORY_ACCESS, true)
                    .field(RTSP_PERM_MEDIA_FACTORY_CONSTRUCT, true)
                    .build(),
            );
        }
        // During auth, first it binds anonymously. At this point it checks
        // RTSP_PERM_MEDIA_FACTORY_ACCESS to see if anyone can connect
        // This is done before the auth token is loaded, possibliy an upstream bug there
        // After checking RTSP_PERM_MEDIA_FACTORY_ACCESS anonymously
        // It loads the auth token of the user and checks that users
        // RTSP_PERM_MEDIA_FACTORY_CONSTRUCT allowing them to play
        // As a result of this we must ensure that if anonymous is not granted RTSP_PERM_MEDIA_FACTORY_ACCESS
        // As a part of permitted users then we must allow it to access
        // at least RTSP_PERM_MEDIA_FACTORY_ACCESS but not RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // Watching Actually happens during RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        // So this should be OK to do.
        // FYI: If no RTSP_PERM_MEDIA_FACTORY_ACCESS then server returns 404 not found
        //      If yes RTSP_PERM_MEDIA_FACTORY_ACCESS but no RTSP_PERM_MEDIA_FACTORY_CONSTRUCT
        //        server returns 401 not authourised
        if !permitted_roles
            .iter()
            .map(|i| i.as_ref())
            .collect::<HashSet<&str>>()
            .contains(&"anonymous")
        {
            self.add_role_from_structure(
                &Structure::builder("anonymous")
                    .field(RTSP_PERM_MEDIA_FACTORY_ACCESS, true)
                    .build(),
            );
        }
    }
}

unsafe impl Send for NeoMediaFactory {}
unsafe impl Sync for NeoMediaFactory {}

#[allow(clippy::type_complexity)]
pub(crate) struct NeoMediaFactoryImpl {
    call_back: Arc<
        Mutex<
            Option<
                Arc<
                    dyn Fn(Element, Option<AudioFormat>) -> AnyResult<Option<Element>>
                        + Send
                        + Sync,
                >,
            >,
        >,
    >,
}

impl Default for NeoMediaFactoryImpl {
    fn default() -> Self {
        debug!("Constructing Factor Impl");
        // Prepare thread that sends data into the appsrcs
        Self {
            call_back: Arc::new(Mutex::new(None)),
        }
    }
}

impl NeoMediaFactoryImpl {
    async fn set_callback<F>(&self, callback: F)
    where
        F: Fn(Element, Option<AudioFormat>) -> AnyResult<Option<Element>> + Send + Sync + 'static,
    {
        self.call_back.lock().await.replace(Arc::new(callback));
    }
    fn build_pipeline(
        &self,
        media: Element,
        audio_format: Option<AudioFormat>,
    ) -> AnyResult<Option<Element>> {
        match self.call_back.blocking_lock().as_ref() {
            Some(call) => {
                let new_media = call(media, audio_format);
                match new_media {
                    Ok(new_media) => Ok(new_media),
                    Err(e) => {
                        log::debug!("Media source is currently restarting: {e:?}");
                        Ok(None)
                    }
                }
            }
            None => Ok(None),
        }
    }
}

impl ObjectImpl for NeoMediaFactoryImpl {}
impl RTSPMediaFactoryImpl for NeoMediaFactoryImpl {
    fn create_element(&self, url: &RTSPUrl) -> Option<Element> {
        // Build the placeholder/real pipeline from the parent's parsed launch
        // line. If we cannot build a real pipeline yet (the camera is
        // restarting / not ready / no callback set), we must NOT return `None`.
        //
        // Returning a NULL element here makes gst-rtsp-server abort the media
        // construction and emit, on *every* client connection:
        //   GLib-GObject-CRITICAL  g_object_force_floating: assertion 'G_IS_OBJECT (object)' failed
        //   GStreamer-RTSP-Server-CRITICAL  could not create element
        // The connecting client then receives a 503 and immediately retries,
        // producing a tight error-spam loop. Instead we fall back to the
        // parent's "Stream not Ready" splash pipeline so the client stays
        // connected and simply sees the placeholder until the real stream
        // becomes available.
        let orig = self.parent_create_element(url)?;
        self.build_pipeline(orig, requested_audio_format(url))
            .ok()
            .flatten()
            .or_else(|| self.parent_create_element(url))
    }
}

/// The audio format this client asked for with `?audio=` on its URL, if it
/// asked for one we understand.
///
/// This is the only per-client negotiation RTSP really offers: the protocol
/// has no way for a client to say what it can decode, but it can ask for a
/// different resource. The mount is matched on the path alone, so the query
/// rides along without disturbing it.
fn requested_audio_format(url: &RTSPUrl) -> Option<AudioFormat> {
    let requested = audio_query_param(url.request_uri().as_str())?;
    match AudioFormat::from_request(&requested) {
        Some(format) => {
            log::debug!("Client asked for audio_format={format}");
            Some(format)
        }
        None => {
            log::warn!(
                "Ignoring unknown `?audio={requested}` on the request URL; \
                 expected one of latm, pcm or all"
            );
            None
        }
    }
}

/// Pull the `audio` parameter out of a request URI's query string.
fn audio_query_param(uri: &str) -> Option<String> {
    let (_, query) = uri.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        key.eq_ignore_ascii_case("audio")
            .then(|| value.trim().to_string())
    })
}

#[object_subclass]
impl ObjectSubclass for NeoMediaFactoryImpl {
    const NAME: &'static str = "NeoMediaFactory";
    type Type = super::NeoMediaFactory;
    type ParentType = RTSPMediaFactory;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// The whole path a client's `?audio=` takes: RTSP URL in, pipeline
    /// callback out. The parsing is tested below; this is the wiring.
    #[test]
    fn the_requested_format_reaches_the_pipeline_callback() {
        gstreamer::init().expect("gstreamer should initialise");
        if gstreamer::ElementFactory::find("videotestsrc").is_none() {
            eprintln!("skipping: the placeholder pipeline needs videotestsrc");
            return;
        }

        let seen: Arc<StdMutex<Vec<Option<AudioFormat>>>> = Arc::new(StdMutex::new(vec![]));
        let recorder = seen.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime should build");
        let factory = runtime
            .block_on(NeoMediaFactory::new_with_callback(
                move |element, audio_format| {
                    recorder.lock().unwrap().push(audio_format);
                    Ok(Some(element))
                },
            ))
            .expect("factory should build");

        let describe = |uri: &str| {
            let (_, url) = RTSPUrl::parse(uri);
            let url = url.expect("test url should parse");
            let _ = factory.create_element(&url);
        };

        describe("rtsp://host:8554/Camera01/mainStream");
        describe("rtsp://host:8554/Camera01/mainStream?audio=pcm");
        describe("rtsp://host:8554/Camera01/mainStream?audio=latm");
        // Unrecognised: fall back to whatever the camera is configured for
        // rather than inventing a format.
        describe("rtsp://host:8554/Camera01/mainStream?audio=opus");

        assert_eq!(
            *seen.lock().unwrap(),
            vec![None, Some(AudioFormat::Pcm), Some(AudioFormat::Latm), None]
        );
    }

    #[test]
    fn the_audio_parameter_is_read_off_the_request_url() {
        let param = |uri| audio_query_param(uri);

        assert_eq!(param("rtsp://host/cam/mainStream"), None);
        assert_eq!(
            param("rtsp://host/cam/mainStream?audio=pcm").as_deref(),
            Some("pcm")
        );
        // It need not be the only parameter, or the first.
        assert_eq!(
            param("rtsp://host/cam?foo=1&audio=latm&bar=2").as_deref(),
            Some("latm")
        );
        // Keys are matched case-insensitively, values are handed on as
        // written for `AudioFormat::from_request` to interpret.
        assert_eq!(param("rtsp://host/cam?Audio=ALL").as_deref(), Some("ALL"));
        // A stream path that merely contains the word must not count.
        assert_eq!(param("rtsp://host/cam/audio=pcm"), None);
        // Malformed queries are simply absent, not a panic.
        assert_eq!(param("rtsp://host/cam?audio"), None);
        assert_eq!(param("rtsp://host/cam?"), None);
    }
}
