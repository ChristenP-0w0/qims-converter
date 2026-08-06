//! `GET /notifications/stream` — live push channel (Server-Sent Events).
//!
//! SSE rather than a raw WebSocket on purpose: the backend is loopback-only
//! behind the authenticating Next.js proxy, and Next rewrites cannot carry a
//! WebSocket upgrade — an SSE stream is a plain long-lived HTTP response, so
//! it rides the existing authenticated `/backend` route untouched, and the
//! browser's `EventSource` reconnects automatically.
//!
//! The stream carries no data — each event is just "something changed for
//! you", and the client refetches the list over the normal endpoint.

use std::convert::Infallible;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};

use crate::database::users;

pub async fn stream(
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Without a proxy-asserted identity nothing is addressed to the caller;
    // the stream stays open but silent (keep-alives only).
    let email = users::caller_email(&headers).unwrap_or_default();

    let pokes = BroadcastStream::new(bus_rx()).filter_map(move |msg| match msg {
        Ok(recipient) if !email.is_empty() && recipient == email => {
            Some(Ok(Event::default().data("refresh")))
        }
        // Other people's pokes and lag gaps are simply skipped — the client
        // refetches on every event anyway, so nothing can be missed for long.
        _ => None,
    });

    Sse::new(pokes).keep_alive(
        // Comment frames keep intermediary proxies from idling the stream out.
        KeepAlive::new()
            .interval(Duration::from_secs(25))
            .text("keep-alive"),
    )
}

fn bus_rx() -> tokio::sync::broadcast::Receiver<String> {
    super::bus().subscribe()
}
