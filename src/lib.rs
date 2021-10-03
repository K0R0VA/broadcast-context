use std::sync::Arc;

use actix::{
    dev::{AsyncContextParts, ContextFut, ContextParts, Envelope, Mailbox, ToEnvelope},
    Actor, ActorContext, ActorFuture, ActorState, Addr, AsyncContext, Handler, Message,
    SpawnHandle,
};
use futures::Stream;
use interceptor::registry::Registry;
use rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use tokio::sync::oneshot::Sender;
use tokio::time::Duration;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    error::Error,
    media::{
        rtp::{rtp_codec::RTPCodecType, rtp_receiver::RTCRtpReceiver},
        track::{
            track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter},
            track_remote::TrackRemote,
        },
    },
    peer::{
        configuration::RTCConfiguration, ice::ice_server::RTCIceServer,
        peer_connection::RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

pub struct BroadcastContext<A: Actor<Context = Self>> {
    inner: ContextParts<A>,
}

pub struct LocalTrackMessage {
    local_track: Arc<TrackLocalStaticRTP>,
}

impl Message for LocalTrackMessage {
    type Result = anyhow::Result<()>;
}

impl<A: Actor<Context = Self> + Handler<LocalTrackMessage>> BroadcastContext<A> {
    pub async fn create_with_addr(line: String, actor: A) -> anyhow::Result<(Addr<A>, String)> {
        let mailbox = Mailbox::default();
        let parts = ContextParts::new(mailbox.sender_producer());
        let context = Self {
            inner: parts,
        };
        let addr = context.address();
        let context_fut = ContextFut::new(context, actor, mailbox);
        let mut stream = BroadcastContextFuture::new(context_fut).await?;
        let response = stream.setup(&line).await?;
        tokio::task::spawn_local(async move {
            use futures::stream::StreamExt;
            while let Some(()) = stream.next().await {}
        });
        Ok((addr, response))
    }
}

impl<A> ActorContext for BroadcastContext<A>
where
    A: Actor<Context = Self>,
{
    fn stop(&mut self) {
        self.inner.stop();
    }
    fn terminate(&mut self) {
        self.inner.terminate()
    }
    fn state(&self) -> ActorState {
        self.inner.state()
    }
}

impl<A> AsyncContext<A> for BroadcastContext<A>
where
    A: Actor<Context = Self>,
{
    #[inline]
    fn spawn<F>(&mut self, fut: F) -> SpawnHandle
    where
        F: ActorFuture<A, Output = ()> + 'static,
    {
        self.inner.spawn(fut)
    }

    #[inline]
    fn wait<F>(&mut self, fut: F)
    where
        F: ActorFuture<A, Output = ()> + 'static,
    {
        self.inner.wait(fut)
    }

    #[doc(hidden)]
    #[inline]
    fn waiting(&self) -> bool {
        self.inner.waiting()
            || self.inner.state() == ActorState::Stopping
            || self.inner.state() == ActorState::Stopped
    }

    #[inline]
    fn cancel_future(&mut self, handle: SpawnHandle) -> bool {
        self.inner.cancel_future(handle)
    }

    #[inline]
    fn address(&self) -> Addr<A> {
        self.inner.address()
    }
}

impl<A, M> ToEnvelope<A, M> for BroadcastContext<A>
where
    A: Actor<Context = BroadcastContext<A>> + Handler<M>,
    M: Message + Send + 'static,
    M::Result: Send,
{
    fn pack(msg: M, tx: Option<Sender<M::Result>>) -> Envelope<A> {
        Envelope::new(msg, tx)
    }
}

impl<A> AsyncContextParts<A> for BroadcastContext<A>
where
    A: Actor<Context = Self>,
{
    fn parts(&mut self) -> &mut ContextParts<A> {
        &mut self.inner
    }
}

pub fn encode(b: &str) -> String {
    base64::encode(b)
}

/// decode decodes the input from base64
/// It can optionally unzip the input after decoding
pub fn decode(s: &str) -> anyhow::Result<String> {
    let b = base64::decode(s)?;
    let s = String::from_utf8(b)?;
    Ok(s)
}

pub struct BroadcastContextFuture<A: Actor<Context = BroadcastContext<A>>> {
    fut: ContextFut<A, BroadcastContext<A>>,
    connection: Arc<RTCPeerConnection>,
}

impl<A: Actor<Context = BroadcastContext<A>>> BroadcastContextFuture<A> 
    where A: Handler<LocalTrackMessage> {
    pub async fn new(fut: ContextFut<A, BroadcastContext<A>>) -> anyhow::Result<Self> {
        let mut m = MediaEngine::default();

        m.register_default_codecs()?;

        // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
        // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
        // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
        // for each PeerConnection.
        let mut registry = Registry::new();

        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)?;

        // Create the API object with the MediaEngine
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Prepare the configuration
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let connection = Arc::new(api.new_peer_connection(config).await?);

        Ok(Self {
            connection,
            fut 
        })
    }

    pub async fn setup(&self, line: &str) -> anyhow::Result<String> {
        self.add_transceiver_from_kind().await?;
        self.on_track().await;
        let desc_data = decode(line)?;
        let offer = serde_json::from_str::<RTCSessionDescription>(&desc_data)?;
        self.set_remote_description(offer).await?;
        self.on_peer_connection_state_change().await;
        self.create_answer().await?;
        let response = self.get_response().await?;
        Ok(response)
    }

    async fn get_response(&self) -> anyhow::Result<String> {
        let local_desc = self.connection.local_description().await.unwrap();
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = encode(&json_str);
        Ok(b64)
    }

    async fn create_answer(&self) -> anyhow::Result<()> {
        let answer = self.connection.create_answer(None).await?;
        let mut gather_complete = self.connection.gathering_complete_promise().await;
        self.connection.set_local_description(answer).await?;
        let _ = gather_complete.recv().await;
        Ok(())
    }

    async fn set_remote_description(&self, offer: RTCSessionDescription) -> anyhow::Result<()> {
        self.connection.set_remote_description(offer).await?;
        Ok(())
    }

    async fn add_transceiver_from_kind(&self) -> anyhow::Result<()> {
        self.connection
            .add_transceiver_from_kind(RTPCodecType::Video, &[])
            .await?;
        Ok(())
    }

    async fn on_track(&self) {
        let actor: Addr<A> = self.fut.address();
        let pc = Arc::clone(&self.connection);
        self.connection
            .on_track(Box::new(
                move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                    if let Some(track) = track {
                        // Send a PLI on an interval so that the publisher is pushing a keyframe every rtcpPLIInterval
                        // This is a temporary fix until we implement incoming RTCP events, then we would push a PLI only when a viewer requests it
                        let media_ssrc = track.ssrc();
                        let pc2 = Arc::clone(&pc);
                        tokio::spawn(async move {
                            let mut result = anyhow::Result::<usize>::Ok(0);
                            while result.is_ok() {
                                let timeout = tokio::time::sleep(Duration::from_secs(3));
                                tokio::pin!(timeout);

                                tokio::select! {
                                    _ = timeout.as_mut() => {
                                        result = pc2.write_rtcp(&PictureLossIndication {
                                                sender_ssrc: 0,
                                                media_ssrc,
                                        }).await;
                                    }
                                };
                            }
                        });

                        let actor = actor.clone();

                        tokio::spawn(async move {
                            // Create Track that we send video back to browser on
                            let local_track = Arc::new(TrackLocalStaticRTP::new(
                                track.codec().await.capability.clone(),
                                "video".to_owned(),
                                "webrtc-rs".to_owned(),
                            ));

                            let _ = actor
                                .send(LocalTrackMessage {
                                    local_track: local_track.clone(),
                                })
                                .await;

                            // Read RTP packets being sent to webrtc-rs
                            while let Ok((rtp, _)) = track.read_rtp().await {
                                if let Err(err) = local_track.write_rtp(&rtp).await {
                                    if !Error::ErrClosedPipe.equal(&err) {
                                        print!(
                                            "output track write_rtp got error: {} and break",
                                            err
                                        );
                                        break;
                                    } else {
                                        print!("output track write_rtp got error: {}", err);
                                    }
                                }
                            }
                        });
                    }
                    Box::pin(async {})
                },
            ))
            .await;
    }

    async fn on_peer_connection_state_change(&self) {
        self.connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                print!("Peer Connection State has changed: {}\n", s);
                Box::pin(async {})
            }))
            .await;
    }
}

impl<A: Actor<Context = BroadcastContext<A>>> Stream for BroadcastContextFuture<A> {
    type Item = ();

    fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use futures::Future;
        let this = self.get_mut();
        if this.fut.alive() {
            let _ = std::pin::Pin::new(&mut this.fut).poll(cx);
            return std::task::Poll::Ready(Some(()));
        }
        std::task::Poll::Ready(None)
    }
}
