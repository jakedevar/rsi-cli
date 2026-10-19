//! #2353: the connection drain closes idle connections at once, and the
//! in-flight call registry follows a tokened request for as long as it runs.
use super::*;
use rsi_common::types::SessionKind;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// An idle connection and a push stream close when the drain starts; a
/// connection that has begun a request (any byte received) is kept for the
/// caller's drain deadline, as before.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn connection_drain_closes_idle_and_push_connections_but_not_a_begun_request() {
    let fixture = recursive_dag_rpc_fixture();
    let server = &fixture.server;
    let (mut idle_client, idle_stream) = UnixStream::pair().unwrap();
    let (mut stream_client, stream_stream) = UnixStream::pair().unwrap();
    let (mut begun_client, begun_stream) = UnixStream::pair().unwrap();

    let idle = server.handle_connection(idle_stream);
    let push = server.handle_connection(stream_stream);
    // A request begun but not finished keeps its connection past the drain.
    let begun = tokio::time::timeout(
        Duration::from_millis(500),
        server.handle_connection(begun_stream),
    );
    let drive = async {
        let subscribe = RpcRequest::new("Subscribe", serde_json::json!({"event_types": []}));
        stream_client
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut ack = String::new();
        tokio::io::BufReader::new(&mut stream_client)
            .read_line(&mut ack)
            .await
            .unwrap();
        assert!(ack.contains("\"ok\":true"), "subscribed: {ack}");
        begun_client.write_all(b"{").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.begin_connection_drain();
    };
    let (idle, push, begun, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), idle),
        tokio::time::timeout(Duration::from_secs(5), push),
        begun,
        drive
    );
    assert!(
        idle.expect("an idle connection closes at the drain")
            .is_ok()
    );
    assert!(push.expect("a push stream ends at the drain").is_ok());
    assert!(begun.is_err(), "a begun request holds its connection");

    let mut byte = [0_u8; 1];
    assert_eq!(
        idle_client.read(&mut byte).await.unwrap(),
        0,
        "idle client sees EOF"
    );
    assert_eq!(
        stream_client.read(&mut byte).await.unwrap(),
        0,
        "push client sees EOF"
    );
}

/// A subscriber that has stopped reading fills the socket buffer and parks the
/// push stream inside `write_all`; the connection drain must cut that write
/// too, or the stalled reader holds the whole drain limit (#2383).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_push_stream_stalled_on_a_reader_that_stopped_reading_ends_at_the_drain_2383() {
    let fixture = recursive_dag_rpc_fixture();
    let server = &fixture.server;
    let bus = fixture.manager.event_bus();
    let (mut client, stream) = UnixStream::pair().unwrap();
    let connection = tokio::time::timeout(Duration::from_secs(5), server.handle_connection(stream));
    let drive = async {
        let subscribe = RpcRequest::new("Subscribe", serde_json::json!({"event_types": []}));
        client
            .write_all(format!("{}\n", serde_json::to_string(&subscribe).unwrap()).as_bytes())
            .await
            .unwrap();
        let mut ack = String::new();
        tokio::io::BufReader::new(&mut client)
            .read_line(&mut ack)
            .await
            .unwrap();
        assert!(ack.contains("\"ok\":true"), "subscribed: {ack}");
        // The client reads nothing from here on. The stream subscribes right
        // after it sends the ack, so wait for it before publishing.
        while bus.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }
        // Each line is far larger than the socket buffer: the first write
        // cannot complete while nobody reads.
        for _ in 0..2 {
            bus.publish(crate::bus::DaemonEvent::SystemMessage {
                level: "info".to_string(),
                message: "x".repeat(2 * 1024 * 1024),
            });
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        server.begin_connection_drain();
    };
    let (connection, ()) = tokio::join!(connection, drive);
    assert!(
        connection
            .expect("a push write stalled on its reader ends at the drain")
            .is_ok()
    );
    // The client stayed connected the whole time: the stream ended because of
    // the drain, not because the peer went away.
    drop(client);
}

/// A request served before the drain is answered, and the connection then
/// closes at the drain instead of waiting for the next request.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_connection_answers_its_request_then_closes_at_the_drain() {
    let fixture = recursive_dag_rpc_fixture();
    let server = &fixture.server;
    let (client, stream) = UnixStream::pair().unwrap();
    let connection = tokio::time::timeout(Duration::from_secs(5), server.handle_connection(stream));
    let drive = async {
        let (reader, mut writer) = client.into_split();
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let request = RpcRequest::new("GetHealthStatus", serde_json::Value::Null);
        writer
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .await
            .unwrap();
        let reply = lines.next_line().await.unwrap().expect("a reply");
        let reply: RpcResponse = serde_json::from_str(&reply).unwrap();
        assert!(reply.error.is_none(), "{:?}", reply.error);
        server.begin_connection_drain();
        assert!(
            lines.next_line().await.unwrap().is_none(),
            "the connection closes once the drain starts"
        );
    };
    let (connection, ()) = tokio::join!(connection, drive);
    assert!(connection.expect("closed at the drain").is_ok());
}

/// A tokened request is registered as an in-flight control call while its
/// handler runs and unregistered when it returns (#2353).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_tokened_call_is_in_flight_while_the_daemon_serves_it() {
    let fixture = recursive_dag_rpc_fixture();
    let caller = Uuid::new_v4();
    fixture
        .manager
        .store()
        .lock()
        .await
        .insert_session(&mk_agent_test_session(
            caller,
            SessionKind::Task,
            None,
            None,
        ))
        .unwrap();
    let token = crate::test_support::agent_token("connection-drain-fixture");
    fixture
        .manager
        .register_agent_token(token.clone(), caller)
        .await;
    let mut request = RpcRequest::new("AgentGetStatus", serde_json::Value::Null);
    request.session_token = Some(token.into());
    let registry = &fixture.manager.daemon_wait;
    assert_eq!(registry.in_flight(caller), 0);

    // Hold the store: the handler cannot finish, so the call stays in flight.
    let store = fixture.manager.store().lock().await;
    let serve = fixture.server.handle_request_inner(&request);
    let observe = async {
        let mut seen = 0;
        for _ in 0..1000 {
            seen = registry.in_flight(caller);
            if seen == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        drop(store);
        seen
    };
    let (response, seen) = tokio::join!(serve, observe);
    assert_eq!(seen, 1, "the call is registered while its handler runs");
    assert!(matches!(response, HandleResult::Response(_)));
    assert_eq!(
        registry.in_flight(caller),
        0,
        "and unregistered once it returns"
    );
}
