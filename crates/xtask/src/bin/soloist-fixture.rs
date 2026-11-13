//! Local player fixture. It never contacts Spotify or opens an audio device.
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

struct Player {
    uri: String,
    status: String,
    position: f64,
    at: Instant,
    duration: u64,
    volume: u64,
    active: bool,
}
impl Player {
    fn snapshot(&self) -> Value {
        json!({"type":"playback_state","status":self.status,"is_active":self.active,"volume":self.volume,
            "item":{"uri":self.uri,"decorations":{"identity":{"name":if self.uri.ends_with("first") {"Morning Light"} else {"Last Spotify Song"}},"playback":{"duration_ms":self.duration}}},
            "position":{"position_ms":self.position as u64,"timestamp_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,"speed":if self.status == "playing" {1} else {0}}})
    }
}
fn read(path: PathBuf) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
fn write(path: PathBuf, value: Value) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("Soloist 1.3.10.0 fixture");
        return Ok(());
    }
    let data = PathBuf::from(
        args.windows(2)
            .find(|pair| pair[0] == "-D")
            .context("Missing fixture directory")?[1]
            .clone(),
    );
    std::fs::create_dir_all(&data)?;
    std::fs::write(data.join("soloist.pid"), std::process::id().to_string())?;
    if let Some(delay) = read(data.join("fixture-startup-delay.json")).and_then(|v| v.as_f64()) {
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    std::fs::write(
        data.join("ws.port"),
        listener.local_addr()?.port().to_string(),
    )?;
    let mut state = Player {
        uri: String::new(),
        status: "idle".into(),
        position: 0.0,
        at: Instant::now(),
        duration: 1200,
        volume: 37,
        active: true,
    };
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            continue;
        };
        if socket
            .send(Message::Text(
                json!({"type":"auth_state","logged_in":true,"is_active":state.active})
                    .to_string()
                    .into(),
            ))
            .await
            .is_err()
        {
            continue;
        }
        let mut clock = tokio::time::interval(Duration::from_millis(10));
        loop {
            let response = tokio::select! {_ = clock.tick() => None, message = socket.next() => match message {Some(Ok(Message::Text(text))) => serde_json::from_str::<Value>(&text).ok(),Some(Ok(Message::Close(_)))|None|Some(Err(_)) => break,_=>continue}};
            if let Some(remote) = read(data.join("fixture-playback.json")) {
                let _ = std::fs::remove_file(data.join("fixture-playback.json"));
                if let Some(value) = remote["active"].as_bool() {
                    state.active = value;
                }
                if let Some(value) = remote["uri"].as_str() {
                    state.uri = value.into();
                }
                if let Some(value) = remote["status"].as_str() {
                    state.status = value.into();
                }
                if let Some(value) = remote["position"].as_f64() {
                    state.position = value;
                }
                if let Some(value) = remote["duration"].as_u64() {
                    state.duration = value;
                }
                state.at = Instant::now();
                if socket
                    .send(Message::Text(
                        json!({"type":"device_changed","is_active":state.active})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
                if socket
                    .send(Message::Text(state.snapshot().to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            if state.status == "playing" {
                state.position += state.at.elapsed().as_secs_f64() * 1000.0;
            }
            state.at = Instant::now();
            if state.status == "playing" && state.position >= state.duration as f64 {
                state.status = "idle".into();
                state.position = state.duration as f64;
                if socket
                    .send(Message::Text(state.snapshot().to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            write(
                data.join("fixture-state.json"),
                json!({"uri":state.uri,"status":state.status,"position":state.position,"active":state.active,"volume":state.volume}),
            )?;
            let Some(request) = response else { continue };
            let Some(name) = request["command"].as_str() else {
                continue;
            };
            let mut messages = Vec::new();
            match name {
                "get_state" => {messages.push(state.snapshot());},
                "set_shuffle" => messages.push(json!({"type":"playback_state","status":"playing","is_active":true,"item":{"uri":"spotify:track:first","decorations":{"playback":{"duration_ms":state.duration}}}})),
                "set_repeat_context" => messages.push(json!({"type":"track_changed","item":{"uri":"spotify:track:restored-context"}})),
                "activate" => {state.active=true;messages.push(json!({"type":"device_changed","is_active":true}));},
                "set_volume" => {state.volume=request["volume"].as_u64().unwrap_or(0);messages.push(json!({"type":"volume_changed","volume":state.volume}));},
                "pause" => {state.status="paused".into();messages.push(json!({"type":"playback_changed","status":"paused"}));},
                "play" => {if let Some(uri) = request["uri"].as_str() {state.uri=uri.into();state.position=0.0;}state.status="playing".into();messages.push(state.snapshot());},
                "seek" => {state.position=request["position_ms"].as_f64().unwrap_or(0.0);messages.push(state.snapshot());},
                _ => {},
            }
            let mut failed = false;
            for value in messages {
                if socket
                    .send(Message::Text(value.to_string().into()))
                    .await
                    .is_err()
                {
                    failed = true;
                    break;
                }
            }
            if failed {
                break;
            }
            if name != "get_state" {
                let delays = read(data.join("fixture-delays.json")).unwrap_or_default();
                let delay = delays[format!("{name}:{}", request["uri"].as_str().unwrap_or(""))]
                    .as_f64()
                    .or_else(|| delays[name].as_f64())
                    .unwrap_or(0.0);
                if delay > 0.0 {
                    write(
                        data.join("fixture-pending.json"),
                        json!({"command":name,"uri":request["uri"]}),
                    )?;
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                }
                if socket
                    .send(Message::Text(
                        json!({"type":"command_result","command":name})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}
