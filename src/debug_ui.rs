use crate::engine::Gateway;
use crate::monitor::DebugWsMessage;
use axum::extract::State;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::Html;
use axum::response::IntoResponse;
use axum::response::Response;
use std::sync::Arc;
use tokio::sync::broadcast;

const DEBUG_HTML: &str = include_str!("debug.html");

pub async fn debug_index() -> Html<&'static str> {
    Html(DEBUG_HTML)
}

pub async fn debug_ws(State(gateway): State<Arc<Gateway>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .on_upgrade(move |socket| debug_socket(socket, gateway))
        .into_response()
}

async fn debug_socket(mut socket: WebSocket, gateway: Arc<Gateway>) {
    let mut receiver = gateway.subscribe_monitor();
    let snapshot = gateway.debug_snapshot();
    for message in snapshot.messages {
        if !send_message(&mut socket, &message).await {
            return;
        }
    }

    loop {
        match receiver.recv().await {
            Ok(update) if update.sequence <= snapshot.last_sequence => {}
            Ok(update) => {
                for message in update.messages {
                    if !send_message(&mut socket, &message).await {
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn send_message(socket: &mut WebSocket, message: &DebugWsMessage) -> bool {
    let Ok(payload) = serde_json::to_string(message) else {
        return true;
    };
    socket.send(Message::Text(payload.into())).await.is_ok()
}
