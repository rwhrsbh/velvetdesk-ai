//! One round of sync, over a relayed connection.
//!
//! Direct device-to-device would be nicer and is rarely possible: operators
//! sit behind carrier NAT with no address anyone can dial. So both devices
//! dial out to the same relay, which forwards sealed frames between the two
//! members of a room and understands none of them.
//!
//! The round is symmetric — both sides say what they hold, ask for what they
//! are behind on, and answer what they are asked for — so there is no leader
//! to elect and no order to get wrong.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::pair::Pairing;
use super::wire::{open, seal, Msg};
use super::{apply_item, digest, read_item, wanted, Applied, Report};
use crate::error::{AppError, Result};
use crate::storage::Paths;

/// How long a round may run once the other device has shown up.
const ROUND_LIMIT: Duration = Duration::from_secs(300);

/// The relay address a pairing meets at, as a websocket URL.
fn relay_url(pairing: &Pairing) -> Result<String> {
    let base = pairing.relay.trim_end_matches('/');
    let base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })
        .or_else(|| {
            (base.starts_with("ws://") || base.starts_with("wss://")).then(|| base.to_string())
        })
        .ok_or_else(|| AppError::Invalid(format!("sync: {base} is not an address")))?;
    // The gateway serves the API under /v1 and the relay beside it, so a
    // pairing made from the provider's base URL still finds the right door.
    let base = base.trim_end_matches("/v1");
    Ok(format!("{base}/sync/ws?room={}", pairing.room))
}

/// Run one round against whoever else is in the room.
///
/// This is Pro sync: nothing is stored on the way, so it happens only while
/// both machines are on. The device waits in the room up to `wait_for_peer`
/// for the other one to arrive (the automatic loop waits minutes, the button
/// seconds), and `kick` ends that wait early. `None` when nobody came.
///
/// Both sides greet on arrival, but the one already waiting greeted an empty
/// room, so a newcomer's greeting is answered with our own: without it the
/// newcomer never learnt what we hold and the exchange went one way only.
pub async fn run_round(
    paths: &Paths,
    pairing: &Pairing,
    license: &str,
    wait_for_peer: Duration,
    kick: Option<&tokio::sync::Notify>,
) -> Result<Option<Report>> {
    let secret = pairing.secret()?;
    let url = relay_url(pairing)?;

    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        )
        .map_err(|err| AppError::Invalid(format!("sync: {err}")))?;
    if !license.trim().is_empty() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", license.trim())
                .parse()
                .map_err(|_| AppError::Invalid("sync: the licence is not a header".into()))?,
        );
    }

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(refused)?;
    let (mut sink, mut stream) = socket.split();

    let mut report = Report::default();
    let mine = digest(paths)?;
    let hello = seal(
        &secret,
        &Msg::Hello {
            device_id: pairing.device_id.clone(),
            digest: mine.clone(),
        },
    )?;
    sink.send(WsMessage::Binary(hello.clone()))
        .await
        .map_err(lost)?;

    let mut met_peer = false;
    let mut greeted: HashSet<String> = HashSet::new();
    let mut asked: HashSet<String> = HashSet::new();
    let mut we_are_done = false;
    let mut they_are_done = false;
    let waiting_since = std::time::Instant::now();
    let mut started = std::time::Instant::now();

    loop {
        if we_are_done && they_are_done {
            break;
        }
        let deadline = if met_peer {
            started + ROUND_LIMIT
        } else {
            waiting_since + wait_for_peer
        };
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let next = match kick {
            // Only an idle wait gives way; a round under way finishes.
            Some(kick) if !met_peer => tokio::select! {
                frame = tokio::time::timeout(left, stream.next()) => Some(frame),
                _ = kick.notified() => None,
            },
            _ => Some(tokio::time::timeout(left, stream.next()).await),
        };
        let frame = match next {
            None => break,
            Some(Err(_)) => break,
            Some(Ok(None)) => break,
            Some(Ok(Some(Err(err)))) => return Err(lost(err)),
            Some(Ok(Some(Ok(WsMessage::Binary(bytes))))) => bytes,
            // Anything that is not a sealed frame is the relay talking to
            // itself; a ping is answered by the library.
            Some(Ok(Some(Ok(_)))) => continue,
        };

        let message = match open(&secret, &frame) {
            Ok(message) => message,
            Err(_) => {
                // A frame we cannot open came from someone who is not paired
                // with us. Ignore it: there is nothing to say to them.
                report.rejected += 1;
                continue;
            }
        };

        match message {
            Msg::Hello { device_id, digest } => {
                if device_id == pairing.device_id {
                    continue;
                }
                if !met_peer {
                    met_peer = true;
                    started = std::time::Instant::now();
                }
                if greeted.insert(device_id.clone()) {
                    sink.send(WsMessage::Binary(hello.clone()))
                        .await
                        .map_err(lost)?;
                }
                if asked.insert(device_id) {
                    let keys = wanted(&mine, &digest);
                    sink.send(WsMessage::Binary(seal(&secret, &Msg::Want { keys })?))
                        .await
                        .map_err(lost)?;
                }
            }
            Msg::Want { keys } => {
                if we_are_done {
                    continue;
                }
                met_peer = true;
                let mut sent: Vec<String> = vec![];
                for key in keys {
                    let Ok(body) = read_item(paths, &key) else {
                        continue;
                    };
                    sink.send(WsMessage::Binary(seal(
                        &secret,
                        &Msg::Item {
                            key: key.clone(),
                            body,
                        },
                    )?))
                    .await
                    .map_err(lost)?;
                    sent.push(key);
                    report.pushed += 1;
                }
                // The peer now holds what we hold for these records. Saying so
                // is what keeps its next edit an update rather than a conflict.
                super::note_agreed(paths, &sent)?;
                sink.send(WsMessage::Binary(seal(
                    &secret,
                    &Msg::Done {
                        report: report.clone(),
                    },
                )?))
                .await
                .map_err(lost)?;
                we_are_done = true;
            }
            Msg::Item { key, body } => match apply_item(paths, &key, &body) {
                Ok(Applied::Written) => report.pulled += 1,
                Ok(Applied::Conflicted) => {
                    report.pulled += 1;
                    report.conflicts += 1;
                }
                Ok(Applied::Kept) => {}
                Err(err) => {
                    log::warn!("sync: refused {key}: {err}");
                    report.rejected += 1;
                }
            },
            Msg::Done { .. } => they_are_done = true,
        }
    }

    let _ = sink.send(WsMessage::Close(None)).await;
    if !met_peer {
        return Ok(None);
    }
    report.finished_at = Some(chrono::Utc::now());
    super::write_report(paths, &report)?;
    Ok(Some(report))
}

/// The connection dropped mid-round.
fn lost(err: tokio_tungstenite::tungstenite::Error) -> AppError {
    log::warn!("sync: {err}");
    AppError::message("sync.offline", serde_json::json!({}))
}

/// The relay turned the connection away, or could not be reached at all.
fn refused(err: tokio_tungstenite::tungstenite::Error) -> AppError {
    use tokio_tungstenite::tungstenite::Error as WsError;
    match err {
        WsError::Http(response) => {
            let reason = response
                .body()
                .as_deref()
                .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
                .and_then(|body| body["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or_else(|| response.status().to_string());
            AppError::message("sync.refused", serde_json::json!({ "reason": reason }))
        }
        other => {
            log::warn!("sync: cannot reach the relay: {other}");
            AppError::message("sync.offline", serde_json::json!({}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing(relay: &str) -> Pairing {
        Pairing {
            device_id: "dev".into(),
            key: String::new(),
            room: "ROOM".into(),
            relay: relay.into(),
            auto: true,
        }
    }

    /// The pairing is usually made from the provider's base URL, which ends in
    /// /v1; the relay sits beside the API, not under it.
    #[test]
    fn the_relay_url_is_built_from_the_gateway_address() {
        assert_eq!(
            relay_url(&pairing("https://cloud.velvetdesk.ai/v1")).unwrap(),
            "wss://cloud.velvetdesk.ai/sync/ws?room=ROOM"
        );
        assert_eq!(
            relay_url(&pairing("http://127.0.0.1:8787")).unwrap(),
            "ws://127.0.0.1:8787/sync/ws?room=ROOM"
        );
        assert_eq!(
            relay_url(&pairing("wss://relay.example/")).unwrap(),
            "wss://relay.example/sync/ws?room=ROOM"
        );
    }

    #[test]
    fn an_address_that_is_not_one_is_refused() {
        assert!(relay_url(&pairing("cloud.velvetdesk.ai")).is_err());
    }

    /// A relay like the gateway's: frames sent while nobody else is in the
    /// room are dropped, not kept. The device already waiting greeted an empty
    /// room, so this is where a late arrival used to get nothing back.
    async fn relay_for_two() -> String {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (a, _) = listener.accept().await.unwrap();
            let a = tokio_tungstenite::accept_async(a).await.unwrap();
            let (mut a_tx, mut a_rx) = a.split();
            // Alone in the room: whatever A says goes nowhere.
            let b = loop {
                tokio::select! {
                    _ = a_rx.next() => continue,
                    accepted = listener.accept() => break accepted.unwrap().0,
                }
            };
            let b = tokio_tungstenite::accept_async(b).await.unwrap();
            let (mut b_tx, mut b_rx) = b.split();
            let a_to_b = async {
                while let Some(Ok(frame)) = a_rx.next().await {
                    if frame.is_binary() && b_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            };
            let b_to_a = async {
                while let Some(Ok(frame)) = b_rx.next().await {
                    if frame.is_binary() && a_tx.send(frame).await.is_err() {
                        break;
                    }
                }
            };
            tokio::join!(a_to_b, b_to_a);
        });
        format!("http://{addr}")
    }

    fn device(tag: &str, relay: &str, model: &str) -> (Paths, Pairing) {
        let dir = std::env::temp_dir().join(format!(
            "velvetdesk-live-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let paths = Paths::new(dir).unwrap();
        paths
            .scope(model)
            .unwrap()
            .write_profile(&crate::models::Profile::new(model.into(), tag.into()))
            .unwrap();
        let secret = crate::sync::pair::secret_from_license("VD.test.licence");
        let pairing = Pairing {
            device_id: tag.into(),
            key: base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, secret),
            room: crate::sync::pair::room_for(&secret),
            relay: relay.into(),
            auto: true,
        };
        (paths, pairing)
    }

    #[tokio::test]
    async fn a_device_that_arrives_second_still_trades_both_ways() {
        let relay = relay_for_two().await;
        let (paths_a, pairing_a) = device("a", &relay, "1001");
        let (paths_b, pairing_b) = device("b", &relay, "2002");
        let first = run_round(&paths_a, &pairing_a, "", Duration::from_secs(10), None);
        let second = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            run_round(&paths_b, &pairing_b, "", Duration::from_secs(10), None).await
        };
        let (a, b) = tokio::join!(first, second);
        let a = a.unwrap().expect("A met B");
        let b = b.unwrap().expect("B met A");
        assert_eq!((a.pulled, a.pushed), (1, 1));
        assert_eq!((b.pulled, b.pushed), (1, 1));
        assert!(crate::sync::digest(&paths_a).unwrap().len() == 2);
        assert!(crate::sync::digest(&paths_b).unwrap().len() == 2);
    }

    #[tokio::test]
    async fn nobody_in_the_room_is_none_and_a_kick_ends_the_wait() {
        let relay = relay_for_two().await;
        let (paths, pairing) = device("alone", &relay, "3003");
        let kick = tokio::sync::Notify::new();
        let started = std::time::Instant::now();
        let waiting = run_round(&paths, &pairing, "", Duration::from_secs(30), Some(&kick));
        let kicker = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            kick.notify_one();
        };
        let (outcome, ()) = tokio::join!(waiting, kicker);
        assert!(outcome.unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
