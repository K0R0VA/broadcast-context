use std::{
    sync::{Arc, Weak},
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
    peer_connection: RTCPeerConnection,
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
        let peer_connection = api.new_peer_connection(config).await?;

        Ok(Self {
            peer_connection,
            description,
        })
    }

    pub async fn get_response(&self, message: Arc<TrackLocalStaticRTP>) -> anyhow::Result<String> {
        let rtp_sender = self
            .peer_connection
            .add_track(Arc::clone(&message) as Arc<dyn TrackLocal + Send + Sync>)
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
        self.peer_connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                print!("Peer Connection State has changed: {}\n", s);
                Box::pin(async {})
            }))
            .await;
        let local_description = Arc::try_unwrap(
            self.description
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("local description is empty"))?,
        ).map_err(|_| anyhow::anyhow!("unwraped local_description"))?;
        // Set the remote SessionDescription
        self.peer_connection
            .set_remote_description(local_description)
            .await?;

        // Create an answer
        let answer = self.peer_connection.create_answer(None).await?;

        // Create channel that is blocked until ICE Gathering is complete
        let mut gather_complete = self.peer_connection.gathering_complete_promise().await;

        // Sets the LocalDescription, and starts our UDP listeners
        self.peer_connection.set_local_description(answer).await?;

        // Block until ICE Gathering is complete, disabling trickle ICE
        // we do this because we only can exchange one signaling message
        // in a production application you should exchange ICE Candidates via OnICECandidate
        let _ = gather_complete.recv().await;

        let local_description = self.peer_connection.local_description().await;
        let local_description =
            local_description.ok_or_else(|| anyhow::anyhow!("not found local description"))?;
        let json_str = serde_json::to_string(&local_description)?;
        Ok(json_str)
    }
}
