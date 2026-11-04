mod common;

use futures::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tiletopia_server::realtime::{MAX_CLIENT_MESSAGE_LEN, MAX_MESSAGE_LEN, MAX_USER_ID_JSON_LEN};
use tiletopia_server::router;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{self, header::AUTHORIZATION};
use tokio_tungstenite::tungstenite::{self, Message};

async fn serve() -> SocketAddr {
    let app = router(common::test_state().await);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn room_request(addr: SocketAddr, sub: &str) -> http::Request<()> {
    let mut request = format!("ws://{addr}/api/v1/realtime/room-1")
        .into_client_request()
        .unwrap();
    let bearer = format!("Bearer {}", common::token(sub, "viewer"));
    request
        .headers_mut()
        .insert(AUTHORIZATION, bearer.parse().unwrap());
    request
}

#[tokio::test]
async fn realtime_answers_400_to_a_sub_over_the_json_limit() {
    let addr = serve().await;

    let over_in_raw_bytes = "x".repeat(MAX_USER_ID_JSON_LEN + 1);
    // serde_json escapes a quote to two bytes
    let over_only_once_escaped = "\"".repeat(MAX_USER_ID_JSON_LEN / 2 + 1);
    for sub in [over_in_raw_bytes, over_only_once_escaped] {
        match connect_async(room_request(addr, &sub)).await {
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 400, "{} bytes", sub.len());
            }
            Err(error) => panic!("expected an http rejection, got {error:?}"),
            Ok(_) => panic!("a {} byte sub must not upgrade", sub.len()),
        }
    }

    let at_limit = "x".repeat(MAX_USER_ID_JSON_LEN);
    let (_socket, response) = connect_async(room_request(addr, &at_limit)).await.unwrap();
    assert_eq!(response.status().as_u16(), 101);
}

#[tokio::test]
async fn realtime_relayed_chat_line_stays_within_the_message_limit() {
    let addr = serve().await;
    let longest_sub = "x".repeat(MAX_USER_ID_JSON_LEN);
    let (mut sender, _) = connect_async(room_request(addr, &longest_sub))
        .await
        .unwrap();
    let (mut receiver, _) = connect_async(room_request(addr, "receiver")).await.unwrap();

    let chat = |message: &str| {
        serde_json::json!({
            "type": "Chat",
            "user_id": "",
            "user_name": "Ann",
            "message": message,
            "timestamp": "2026-11-03T00:00:00Z",
        })
        .to_string()
    };
    let chat_of_len = |len: usize| chat(&"x".repeat(len - chat("").len()));

    sender
        .send(Message::Text(chat_of_len(MAX_CLIENT_MESSAGE_LEN).into()))
        .await
        .unwrap();
    let relayed = receiver.next().await.unwrap().unwrap();
    let line = relayed.to_text().unwrap();
    let stamped: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(stamped["user_id"], longest_sub.as_str());
    assert_eq!(line.len(), MAX_MESSAGE_LEN);

    let echo = sender.next().await.unwrap().unwrap();
    assert_eq!(echo.len(), MAX_MESSAGE_LEN);

    // axum closes the socket on an oversized message, possibly mid-write
    let _ = sender
        .send(Message::Text(
            chat_of_len(MAX_CLIENT_MESSAGE_LEN + 1).into(),
        ))
        .await;
    match sender.next().await {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {}
        Some(Ok(reply)) => panic!(
            "a message that overflows once stamped must end the connection, got a {} byte reply",
            reply.len()
        ),
    }
}
