//! `stream: true` yields a well-formed SSE body terminated by [DONE].
mod common;

fn config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn stream_emits_chunks_role_and_done() {
    let server = common::start(&config(common::free_port()));
    let (status, content_type, body) = common::post_collect(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","stream":true,"messages":[{"role":"user","content":"streamtest"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        content_type.contains("text/event-stream"),
        "wrong SSE content-type: {content_type:?}"
    );
    assert!(
        body.contains("chat.completion.chunk"),
        "no chunk object: {body}"
    );
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "stream not terminated by [DONE]: {body}"
    );
    // Frame ordering: role-prime, then content, then terminator.
    let role = body
        .find(r#""role":"assistant""#)
        .expect("role-prime frame");
    let content = body.find("streamtest").expect("content frame");
    let done = body.find("data: [DONE]").expect("DONE terminator");
    assert!(
        role < content && content < done,
        "frames out of order (role={role}, content={content}, done={done}): {body}"
    );
}

/// §9.2's core invariant: **streaming and chain fallback are mutually exclusive
/// after the first byte.**
///
/// Once a `data:` frame has gone out there is no un-sending it, so a leg that
/// fails *after* relaying a token must end the response — not advance and splice
/// a second backend's answer onto the first one mid-stream. Forced here with a
/// tiny `max_response_bytes`: the cap trips *during* the relay, which is the only
/// cheap way to produce a genuine post-commit failure.
#[test]
fn a_leg_that_fails_after_committing_does_not_fall_through() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let up = common::free_port();
    let listener = TcpListener::bind(("127.0.0.1", up)).expect("bind mock upstream");
    let upstream = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        // One small frame, flushed and left alone long enough that the server
        // reads it by itself: that relay is the commit point. Only then does the
        // blast that trips `max_response_bytes` arrive, so the failure lands
        // strictly after the client has already been written to.
        let _ = sock.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"FIRST\"}}]}\n\n");
        let _ = sock.flush();
        std::thread::sleep(std::time::Duration::from_millis(250));
        for _ in 0..64 {
            if sock
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"AAAAAAAAAAAAAAAA\"}}]}\n\n",
                )
                .is_err()
            {
                return;
            }
        }
        let _ = sock.write_all(b"data: [DONE]\n\n");
    });

    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "pair"
env_source = "process"
max_response_bytes = 500

[backends.streamer]
type = "api"
base_url = "http://127.0.0.1:{up}/v1"
model = "streamer"

[backends.fallback]
type = "cli"
bin = "/bin/echo"
args = ["FALLBACK-ANSWER"]

[chains.pair]
order = ["streamer", "fallback"]
"#
    );
    let server = common::start(&cfg);
    let (status, _ct, body) = common::post_collect(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"pair","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "SSE status is committed early: {body}");
    assert!(
        body.contains("FIRST"),
        "the first leg must have relayed a token before failing, or this test is \
         not exercising the post-commit path at all: {body}"
    );
    assert!(
        !body.contains("FALLBACK-ANSWER"),
        "the chain must not splice a second backend onto a committed stream: {body}"
    );
    // The assertion that actually distinguishes a working guard from a missing
    // one. Without it the chain still walks on and dispatches the fallback — and
    // then silently DISCARDS its answer, because the response is already
    // committed to the first leg's frames. The client gets a truncated reply
    // presented as complete, and a backend was spent producing nothing. So the
    // proof is that leg 2 was never dispatched at all.
    let (st, stats) = common::get(&format!("{}/v1/stats", server.base));
    assert_eq!(st, 200, "{stats}");
    let v: serde_json::Value = serde_json::from_str(&stats).expect("stats json");
    assert_eq!(
        v["backends"]["fallback"]["requests"], 0,
        "a committed stream must not go on to dispatch the next leg: {stats}"
    );
    let _ = upstream.join();
}
