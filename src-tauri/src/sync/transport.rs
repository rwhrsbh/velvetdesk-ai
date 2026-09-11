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

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::pair::Pairing;
use super::wire::{open, seal, Msg};
use super::{apply_item, digest, read_item, wanted, Applied, Report};
use crate::error::{AppError, Result};
use crate::storage::Paths;

/// How long to wait for the other device to show up before giving up on this
/// round. A device that is switched off is not an error — the next round will
/// find it.
const WAIT_FOR_PEER: Duration = Duration::from_secs(12);
/// The whole round, however much there is to move.
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
pub async fn run_round(paths: &Paths, pairing: &Pairing, license: &str) -> Result<Report> {
    let secret = pairing.secret()?;
    let url = relay_url(pairing)?;

    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        )
        .map_err(|err| AppError::Http(format!("sync: {err}")))?;
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
        .map_err(|err| AppError::Http(format!("sync: cannot reach the relay: {err}")))?;
    let (mut sink, mut stream) = socket.split();

    let mut report = Report::default();
    let mine = digest(paths)?;
    sink.send(WsMessage::Binary(
        seal(
            &secret,
            &Msg::Hello {
                device_id: pairing.device_id.clone(),
                digest: mine.clone(),
            },
        )?,
    ))
    .await
    .map_err(|err| AppError::Http(format!("sync: {err}")))?;

    let mut met_peer = false;
    let mut we_are_done = false;
    let mut they_are_done = false;
    let started = std::time::Instant::now();

    loop {
        if we_are_done && they_are_done {
            break;
        }
        if started.elapsed() > ROUND_LIMIT {
            break;
        }
        // Before anyone has spoken, a quiet room means nobody else is on.
        let patience = if met_peer { ROUND_LIMIT } else { WAIT_FOR_PEER };
        let next = tokio::time::timeout(patience, stream.next()).await;
        let frame = match next {
            Err(_) => break,
            Ok(None) => break,
            Ok(Some(Err(err))) => return Err(AppError::Http(format!("sync: {err}"))),
            Ok(Some(Ok(WsMessage::Binary(bytes)))) => bytes,
            // Anything that is not a sealed frame is the relay talking to
            // itself; a ping is answered by the library.
            Ok(Some(Ok(_))) => continue,
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
                met_peer = true;
                let keys = wanted(&mine, &digest);
                report.pulled = 0;
                sink.send(WsMessage::Binary(
                    seal(&secret, &Msg::Want { keys })?,
                ))
                .await
                .map_err(|err| AppError::Http(format!("sync: {err}")))?;
            }
            Msg::Want { keys } => {
                met_peer = true;
                for key in keys {
                    let Ok(body) = read_item(paths, &key) else {
                        continue;
                    };
                    sink.send(WsMessage::Binary(
                        seal(&secret, &Msg::Item { key, body })?,
                    ))
                    .await
                    .map_err(|err| AppError::Http(format!("sync: {err}")))?;
                    report.pushed += 1;
                }
                sink.send(WsMessage::Binary(
                    seal(
                        &secret,
                        &Msg::Done {
                            report: report.clone(),
                        },
                    )?,
                ))
                .await
                .map_err(|err| AppError::Http(format!("sync: {err}")))?;
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
    report.finished_at = Some(chrono::Utc::now());
    super::write_report(paths, &report)?;
    Ok(report)
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
}
