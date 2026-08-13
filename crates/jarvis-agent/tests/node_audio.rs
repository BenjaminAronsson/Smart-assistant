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
use jarvis_agent::wake::{WakeGate, WakeWordDetector};
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

/// Fires on the Nth frame it sees, once.
struct FiresOnce {
    at: usize,
    seen: usize,
}

impl WakeWordDetector for FiresOnce {
    fn accept(&mut self, _frame: &[u8]) -> bool {
        self.seen += 1;
        self.seen == self.at
    }
    fn word(&self) -> &str {
        "jarvis"
    }
}

/// The other half of the privacy claim: once the word fires, audio *does* flow
/// — and it is bracketed by a real `voice.stream.start`, not raw binary.
///
/// Without this, "nothing streams before detection" could be satisfied by a
/// node that never streams at all.
#[tokio::test]
async fn a_detection_opens_a_bracketed_stream_and_audio_then_flows() {
    let (address, received) = spawn_recording_daemon().await;
    let speaker = FakeSpeaker::default();
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel(64);
    let gate = WakeGate::new(Box::new(FiresOnce { at: 10, seen: 0 }) as Box<dyn WakeWordDetector>);
    let audio = NodeAudio::new(NodeVoice::new(speaker), frames_rx).with_gate(gate);

    // Loud frames throughout, so nothing looks like end-of-speech.
    tokio::spawn(async move {
        let loud: Vec<u8> = (0..320).flat_map(|_| 8000_i16.to_le_bytes()).collect();
        for _ in 0..40 {
            if frames_tx.send(loud.clone()).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });

    let (tx, rx) = tokio::sync::watch::channel(false);
    let seen = received.clone();
    tokio::spawn(async move {
        // Wait until audio has actually arrived, then stop.
        for _ in 0..200 {
            if seen
                .lock()
                .expect("lock")
                .iter()
                .any(|m| m.starts_with("binary:"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tx.send(true);
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials(address), &NoCompositor, Some(audio), rx),
    )
    .await
    .expect("must shut down cleanly")
    .expect("run");

    let messages = received.lock().expect("lock").clone();
    let first_binary = messages
        .iter()
        .position(|m| m.starts_with("binary:"))
        .expect("audio must flow once the word has fired");
    let start = messages
        .iter()
        .position(|m| m.contains("voice.stream.start"))
        .expect("the stream must be opened with a control frame");
    assert!(
        start < first_binary,
        "audio must be preceded by voice.stream.start, got {messages:?}"
    );
    // The pre-roll means the sentence's beginning survives the detection.
    let binary_count = messages.iter().filter(|m| m.starts_with("binary:")).count();
    assert!(
        binary_count > 1,
        "the pre-roll must be delivered, not just the firing frame"
    );
}

/// Records everything the node sends, text and binary.
async fn spawn_recording_daemon() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let received = Arc::new(Mutex::new(Vec::new()));

    let seen = received.clone();
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let Ok(stream) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };
        use futures_util::StreamExt as _;
        let (_sink, mut source) = stream.split();
        while let Some(Ok(message)) = source.next().await {
            let record = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => format!("binary:{}", bytes.len()),
                _ => continue,
            };
            seen.lock().expect("lock").push(record);
        }
    });

    (address, received)
}

/// Fires on any frame loud enough — i.e. exactly the detector that would loop
/// forever on its own speaker if nothing suppressed it.
struct FiresOnAnythingLoud;

impl WakeWordDetector for FiresOnAnythingLoud {
    fn accept(&mut self, frame: &[u8]) -> bool {
        jarvis_agent::aec::frame_energy(frame) > 0.05
    }
    fn word(&self) -> &str {
        "jarvis"
    }
}

/// F8.4's headline claim: **playback does not self-trigger the wake word.**
///
/// The daemon speaks; the node's microphone hears its own speaker at full
/// volume (simulated by feeding loud capture frames throughout); the detector
/// is one that fires on any loud audio. Without suppression this is an infinite
/// loop — the node wakes itself, streams, gets an answer, and wakes itself
/// again. Nothing may be streamed.
#[tokio::test]
async fn the_nodes_own_playback_does_not_trigger_its_wake_word() {
    let (address, received) = spawn_recording_daemon_that_speaks().await;
    let speaker = FakeSpeaker::default();
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel(64);
    let gate = WakeGate::new(Box::new(FiresOnAnythingLoud) as Box<dyn WakeWordDetector>);
    let mut audio = NodeAudio::new(NodeVoice::new(speaker), frames_rx).with_gate(gate);
    // No echo cancellation: the degraded case, where suppression is the only
    // defence there is.
    audio.aec.set_enabled(false);

    tokio::spawn(async move {
        // Let playback genuinely start first. The claim under test is that the
        // node does not trigger on *its own speaker*, which presupposes the
        // speaker is running — audio captured before anything has played is
        // not echo, and firing on it would be correct behaviour.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let loud: Vec<u8> = (0..320).flat_map(|_| 12000_i16.to_le_bytes()).collect();
        for _ in 0..100 {
            if frames_tx.send(loud.clone()).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });

    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let _ = tx.send(true);
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client::run(&credentials(address), &NoCompositor, Some(audio), rx),
    )
    .await
    .expect("must shut down cleanly")
    .expect("run");

    let messages = received.lock().expect("lock").clone();
    assert!(
        !messages.iter().any(|m| m.contains("voice.stream.start")),
        "the node must not wake itself on its own playback; it sent {messages:?}"
    );
    assert!(
        !messages.iter().any(|m| m.starts_with("binary:")),
        "and it must stream nothing"
    );
}

/// Records what the node sends, and speaks continuously so the node is in the
/// "assistant is talking" state throughout.
async fn spawn_recording_daemon_that_speaks() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let received = Arc::new(Mutex::new(Vec::new()));

    let seen = received.clone();
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let Ok(stream) = tokio_tungstenite::accept_async(socket).await else {
            return;
        };
        use futures_util::StreamExt as _;
        let (mut sink, mut source) = stream.split();

        let start = serde_json::json!({
            "v": 1, "seq": 1, "channel": "voice",
            "type": "voice.speak.start",
            "occurredAt": "2026-08-13T00:00:00Z",
            "payload": {
                "utteranceId": "u1", "sampleRateHz": 16000,
                "sampleWidthBytes": 2, "channels": 1
            }
        });
        let _ = sink.send(Message::Text(start.to_string().into())).await;

        // Keep speaking for the duration of the test.
        tokio::spawn(async move {
            let loud: Vec<u8> = (0..320).flat_map(|_| 12000_i16.to_le_bytes()).collect();
            for _ in 0..200 {
                if sink
                    .send(Message::Binary(loud.clone().into()))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });

        while let Some(Ok(message)) = source.next().await {
            let record = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => format!("binary:{}", bytes.len()),
                _ => continue,
            };
            seen.lock().expect("lock").push(record);
        }
    });

    (address, received)
}
