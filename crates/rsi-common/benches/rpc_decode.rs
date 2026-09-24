//! Phase 0.3 — RPC frame decode bench.
//!
//! Measures the hot decode path used by `rsid`'s JSON-RPC server: parse a JSON
//! frame off the socket into `RpcRequest`, then deserialize the typed `params`
//! payload into the method-specific struct (`LaunchSessionParams`,
//! `GetConversationsSinceParams`, etc.).
//!
//! Hermetic: no socket, no daemon. Inputs are static `&[u8]` JSON literals.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use rsi_common::rpc::{GetConversationsSinceParams, LaunchSessionParams, RpcRequest};

const LAUNCH_SESSION_FRAME: &[u8] = br#"{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "LaunchSession",
    "params": {
        "query": "Investigate the JSON-RPC decode hot path and find any bottleneck",
        "working_dir": "/home/jake/rsi",
        "provider": "Claude",
        "model": "claude-opus-4-7",
        "system_prompt": null,
        "session_kind": "Standard",
        "max_retries": 0
    }
}"#;

const LIST_SESSIONS_FRAME: &[u8] = br#"{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "ListSessions"
}"#;

// 8-cursor batched frame — representative of TUI poll fan-out.
const GET_CONVERSATIONS_SINCE_FRAME: &[u8] = br#"{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "GetConversationsSince",
    "params": {
        "requests": [
            {"session_id": "11111111-1111-1111-1111-111111111111", "since_sequence": 0},
            {"session_id": "22222222-2222-2222-2222-222222222222", "since_sequence": 42},
            {"session_id": "33333333-3333-3333-3333-333333333333", "since_sequence": 128},
            {"session_id": "44444444-4444-4444-4444-444444444444", "since_sequence": null},
            {"session_id": "55555555-5555-5555-5555-555555555555", "since_sequence": 1024},
            {"session_id": "66666666-6666-6666-6666-666666666666", "since_sequence": 7},
            {"session_id": "77777777-7777-7777-7777-777777777777", "since_sequence": 0},
            {"session_id": "88888888-8888-8888-8888-888888888888", "since_sequence": 2048}
        ]
    }
}"#;

fn bench_launch_session(c: &mut Criterion) {
    c.bench_function("rpc_decode/LaunchSession", |b| {
        b.iter(|| {
            let req: RpcRequest = serde_json::from_slice(black_box(LAUNCH_SESSION_FRAME))
                .expect("valid LaunchSession frame");
            let params: LaunchSessionParams =
                serde_json::from_value(black_box(req.params)).expect("valid LaunchSessionParams");
            black_box(params);
        });
    });
}

fn bench_list_sessions(c: &mut Criterion) {
    c.bench_function("rpc_decode/ListSessions", |b| {
        b.iter(|| {
            let req: RpcRequest = serde_json::from_slice(black_box(LIST_SESSIONS_FRAME))
                .expect("valid ListSessions frame");
            black_box(req);
        });
    });
}

fn bench_get_conversations_since(c: &mut Criterion) {
    c.bench_function("rpc_decode/GetConversationsSince", |b| {
        b.iter(|| {
            let req: RpcRequest = serde_json::from_slice(black_box(GET_CONVERSATIONS_SINCE_FRAME))
                .expect("valid GetConversationsSince frame");
            let params: GetConversationsSinceParams = serde_json::from_value(black_box(req.params))
                .expect("valid GetConversationsSinceParams");
            black_box(params);
        });
    });
}

criterion_group!(
    benches,
    bench_launch_session,
    bench_list_sessions,
    bench_get_conversations_since
);
criterion_main!(benches);
