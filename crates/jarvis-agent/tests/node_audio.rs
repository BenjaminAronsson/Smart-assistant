//! F8.2 evidence at the socket: spoken audio the daemon sends reaches this
//! node's speaker, and nothing is streamed the other way.
//!
//! The unit tests in `node_voice` cover the decisions; this covers the wiring
//! between them and a real WebSocket — the seam where a channel mix-up or a
//! missed binary arm would otherwise go unnoticed until someone stood in a
//! kitchen.

use std::sync::{Arc, Mutex};

use futures_util::SinkExt as _;
use jarvis_agent::audio::AudioOutput;
use jarvis_agent::client::{self, NodeAudio};
use jarvis_agent::compositor::NoCompositor;
use jarvis_agent::node_voice::NodeVoice;
use jarvis_agent::store::Credentials;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Default)]
struct FakeSpeaker {
    played: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl AudioOutput for FakeSpeaker {
    fn play(&self, frame: &[u8]) -> anyhow::Result<()> {
        self.played.lock().expect("lock").push(frame.to_vec());
        Ok(())
    }
    fn flush(&self) {}
    fn describe(&self) -> String {
        "fake speaker".into()
    }
}

fn credentials(address: std::net::SocketAddr) -> Credentials {
    Credentials {
        server_url: format!("http://{address}"),
        private_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        device_token: "a-token".into(),
        device_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        device_class: "voice-node".into(),
        server_fingerprint: None,
    }
}

/// A daemon that speaks one utterance and then closes.
async fn spawn_speaking_daemon(frames: usize) -> (std::net::SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    // Anything the node sends us — which must be nothing.
    let received = Arc::new(Mutex::new(Vec::new()));

    let seen = received.clone();
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let Ok(mut stream) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };

        let start = serde_json::json!({
            "v": 1, "seq": 1, "channel": "voice",
            "type": "voice.speak.start",
            "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {
                "utteranceId": "u1",
                "sampleRateHz": 16000,
                "sampleWidthBytes": 2,
                "channels": 1
            }
        });
        let _ = stream.send(Message::Text(start.to_string().into())).await;
        for _ in 0..frames {
            let _ = stream.send(Message::Binary(vec![0_u8; 640].into())).await;
        }
        let stop = serde_json::json!({
            "v": 1, "seq": 2, "channel": "voice",
            "type": "voice.speak.stop",
            "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {"utteranceId": "u1", "reason": "completed"}
        });
        let _ = stream.send(Message::Text(stop.to_string().into())).await;

        // Drain anything the node sends until it closes, recording it.
        use futures_util::StreamExt as _;
        while let Some(Ok(message)) = stream.next().await {
            if let Message::Binary(bytes) = message {
                seen.lock().expect("lock").push(bytes.to_vec());
            }
        }
    });

    (address, received)
}

#[tokio::test]
async fn spoken_audio_from_the_daemon_reaches_this_nodes_speaker() {
    let (address, _) = spawn_speaking_daemon(3).await;
    let speaker = FakeSpeaker::default();
    let (_tx_frames, rx_frames) = tokio::sync::mpsc::channel(8);
    let audio = NodeAudio::new(NodeVoice::new(speaker.clone()), rx_frames);

    let (tx, rx) = tokio::sync::watch::channel(false);
    // Stop once the utterance has been delivered.
    let played = speaker.played.clone();
    tokio::spawn(async move {
        while played.lock().expect("lock").len() < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = tx.send(true);
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials(address), &NoCompositor, Some(audio), rx),
    )
    .await
    .expect("the node must play the utterance and then observe shutdown")
    .expect("run");

    assert_eq!(
        speaker.played.lock().expect("lock").len(),
        3,
        "every frame of the utterance must reach the speaker"
    );
}

/// The privacy property M8's decision 3 turns on, asserted at the socket rather
/// than in the client: with no stream open, a node sends the daemon **nothing**,
/// however much the microphone hears.
#[tokio::test]
async fn a_node_streams_nothing_before_a_stream_is_opened() {
    let (address, received) = spawn_speaking_daemon(1).await;
    let speaker = FakeSpeaker::default();
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel(64);
    let audio = NodeAudio::new(NodeVoice::new(speaker.clone()), frames_rx);

    // A microphone that is very much hearing things.
    tokio::spawn(async move {
        for _ in 0..50 {
            if frames_tx.send(vec![7_u8; 640]).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });

    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let _ = tx.send(true);
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials(address), &NoCompositor, Some(audio), rx),
    )
    .await
    .expect("must shut down cleanly")
    .expect("run");

    assert!(
        received.lock().expect("lock").is_empty(),
        "a node with no open stream must send no audio at all; sent {} frames",
        received.lock().expect("lock").len()
    );
}
