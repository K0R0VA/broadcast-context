use std::sync::{Arc, Weak};

use actix::{
    Actor, ActorFutureExt, Addr, AsyncContext, Context, Handler, Message,
    WrapFuture,
};
use interceptor::registry::Registry;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    media::track::track_local::TrackLocal,
    peer::{
        configuration::RTCConfiguration, ice::ice_server::RTCIceServer,
        peer_connection::RTCPeerConnection, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use webrtc::media::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

pub struct Recipient {
    peer_connection: Arc<RTCPeerConnection>,
    description: Weak<RTCSessionDescription>,
}

impl Recipient {
    pub async fn new(key: String) -> anyhow::Result<Self> {
        let recv_only_offer = Arc::new(serde_json::from_str::<RTCSessionDescription>(&key)?);
        let description = Arc::downgrade(&recv_only_offer);

        // Create a MediaEngine object to configure the supported codec
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

        // Create a new RTCPeerConnection
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        Ok(Self {
            peer_connection,
            description,
        })
    }
}

impl Actor for Recipient {
    type Context = Context<Self>;
}

pub struct RecipientResponse(pub String);

impl Message for RecipientResponse {
    type Result = ();
}

pub struct RecipientLocalTrackMessage<A: Actor + Handler<RecipientResponse>> {
    pub address: Addr<A>,
    pub local_track: Arc<TrackLocalStaticRTP>,
}

impl<A: Actor + Handler<RecipientResponse>> Message for RecipientLocalTrackMessage<A> {
    type Result = ();
}

impl<A: Actor<Context = Context<A>> + Handler<RecipientResponse>>
    Handler<RecipientLocalTrackMessage<A>> for Recipient
{
    type Result = ();
    fn handle(
        &mut self,
        msg: RecipientLocalTrackMessage<A>,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        let peer_connection = Arc::clone(&self.peer_connection);
        let description = Arc::clone(&self.description.upgrade().unwrap());
        ctx.wait(
            async move {
                let rtp_sender = peer_connection
                    .add_track(Arc::clone(&msg.local_track) as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;

                // Read incoming RTCP packets
                // Before these packets are returned they are processed by interceptors. For things
                // like NACK this needs to be called.
                tokio::spawn(async move {
                    let mut rtcp_buf = vec![0u8; 1500];
                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                    anyhow::Result::<()>::Ok(())
                });

                // Set the handler for Peer connection state
                // This will notify you when the peer has connected/disconnected
                peer_connection
                    .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                        print!("Peer Connection State has changed: {}\n", s);
                        Box::pin(async {})
                    }))
                    .await;
                let local_description = Arc::try_unwrap(description)
                    .map_err(|_| anyhow::anyhow!("unwraped local_description"))?;
                // Set the remote SessionDescription
                peer_connection
                    .set_remote_description(local_description)
                    .await?;

                // Create an answer
                let answer = peer_connection.create_answer(None).await?;

                // Create channel that is blocked until ICE Gathering is complete
                let mut gather_complete = peer_connection.gathering_complete_promise().await;

                // Sets the LocalDescription, and starts our UDP listeners
                peer_connection.set_local_description(answer).await?;

                // Block until ICE Gathering is complete, disabling trickle ICE
                // we do this because we only can exchange one signaling message
                // in a production application you should exchange ICE Candidates via OnICECandidate
                let _ = gather_complete.recv().await;

                let local_description = peer_connection.local_description().await;
                let local_description = local_description
                    .ok_or_else(|| anyhow::anyhow!("not found local description"))?;
                let json_str = serde_json::to_string(&local_description)?;
                msg.address.send(RecipientResponse(json_str)).await?;
                anyhow::Result::<(), anyhow::Error>::Ok(())
            }
            .into_actor(self)
            .then(|_, _, _| actix::fut::ready(())),
        )
    }
}
