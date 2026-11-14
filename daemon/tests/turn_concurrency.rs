//! A session with a turn already running: refused second turns, steering, queueing and interrupts.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// `turn_complete` means the session is idle: a watcher that sends the next
/// turn the moment it sees one, refusing to queue, is never told the session
/// is busy with the turn that just completed — not even after a goal's last
/// round, which still has the goal's end to write down.
#[tokio::test]
async fn a_turn_sent_on_turn_complete_finds_the_session_free() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("done")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let created = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{}}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();

    let (read, mut write) = UnixStream::connect(&srv.sock).await.unwrap().into_split();
    let mut lines = BufReader::new(read).lines();
    let subscribe = serde_json::json!({
        "jsonrpc": "2.0", "method": "session.subscribe", "id": 1,
        "params": {"session_id": sid},
    });
    write
        .write_all(format!("{subscribe}\n").as_bytes())
        .await
        .unwrap();
    until_response(&mut lines, 1).await;

    for n in 0..30u64 {
        let goal = |params: serde_json::Value, method: &str| {
            serde_json::json!({"jsonrpc": "2.0", "method": method, "id": 1, "params": params})
                .to_string()
        };
        rpc(
            &srv.sock,
            &goal(
                serde_json::json!({"session_id": sid, "action": "clear"}),
                "session.updateGoal",
            ),
        )
        .await;
        let set = rpc(
            &srv.sock,
            &goal(
                serde_json::json!({"session_id": sid, "objective": "tidy up", "max_rounds": 1}),
                "session.setGoal",
            ),
        )
        .await;
        assert!(set["result"].is_object(), "{set}");

        let id = 100 + n;
        let next = serde_json::json!({
            "jsonrpc": "2.0", "method": "session.run_turn", "id": id,
            "params": {"session_id": sid, "message": "next", "turn_id": format!("t{n}"),
                       "on_busy": "reject"},
        });
        let mut sent = false;
        let answer = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let line = lines.next_line().await.unwrap().expect("connection open");
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame["id"] == id {
                    return frame;
                }
                let goal_round_over = frame["params"]["event"]["kind"] == "turn_complete"
                    && frame["params"]["turn_id"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("goal-"));
                if goal_round_over && !sent {
                    sent = true;
                    write
                        .write_all(format!("{next}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        })
        .await
        .expect("the turn sent on turn_complete was answered");
        assert!(answer["result"].is_object(), "round {n}: {answer}");
    }

    srv.stop().await;
}

/// The calls refused while a turn runs are not refused once it has sent
/// `turn_complete`: each is sent the moment a goal's last round completes,
/// while the round is still writing the goal's end down, on a session of its
/// own.
#[tokio::test]
async fn a_change_sent_on_turn_complete_finds_the_session_free() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("done")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;

    let calls = [
        (
            "session.setPrompt",
            serde_json::json!({"append": "Be brief."}),
        ),
        (
            "session.setModel",
            serde_json::json!({"model": "claude-haiku-4-5"}),
        ),
        ("session.inspectContext", serde_json::json!({})),
        ("session.dropMessages", serde_json::json!({"at": []})),
        (
            "session.editMessage",
            serde_json::json!({"at": 0, "text": "rewritten"}),
        ),
        ("session.compact", serde_json::json!({})),
    ];
    let send = |id: u64, method: &str, params: serde_json::Value| {
        format!(
            "{}\n",
            serde_json::json!({"jsonrpc": "2.0", "method": method, "id": id, "params": params})
        )
    };
    for (method, extra) in calls {
        for n in 0..20u64 {
            let created = rpc(
                &srv.sock,
                r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{}}"#,
            )
            .await;
            let sid = created["result"]["session_id"]
                .as_str()
                .unwrap_or_else(|| panic!("{created}"))
                .to_string();
            let (read, mut write) = UnixStream::connect(&srv.sock).await.unwrap().into_split();
            let mut lines = BufReader::new(read).lines();
            let subscribe = send(
                1,
                "session.subscribe",
                serde_json::json!({"session_id": sid}),
            );
            write.write_all(subscribe.as_bytes()).await.unwrap();
            until_response(&mut lines, 1).await;
            let goal = send(
                2,
                "session.setGoal",
                serde_json::json!({"session_id": sid, "objective": "tidy up", "max_rounds": 1}),
            );
            write.write_all(goal.as_bytes()).await.unwrap();

            let mut params = extra.clone();
            params["session_id"] = serde_json::json!(sid);
            let call = send(3, method, params);
            let mut sent = false;
            let answer = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let line = lines.next_line().await.unwrap().expect("connection open");
                    let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if frame["id"] == 3 {
                        return frame;
                    }
                    let goal_round_over = frame["params"]["event"]["kind"] == "turn_complete"
                        && frame["params"]["turn_id"]
                            .as_str()
                            .is_some_and(|t| t.starts_with("goal-"));
                    if goal_round_over && !sent {
                        sent = true;
                        write.write_all(call.as_bytes()).await.unwrap();
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{method} sent on turn_complete was answered"));
            assert_ne!(
                answer["error"]["code"],
                codes::SESSION_BUSY,
                "{method}, round {n}: {answer}"
            );
        }
    }

    srv.stop().await;
}

/// A session runs one turn at a time, and the second caller is told so
/// rather than being served an internal error.
///
/// The ordering matters more than the code: the message must not reach the
/// agent's input queue before the exclusive event channel is claimed, or a
/// rejected second `run_turn` hands it to the turn still running — the caller
/// sees a failure and the message is answered anyway, by the wrong turn.
#[tokio::test]
async fn a_second_turn_on_a_busy_session_is_refused_and_its_message_is_not_swallowed() {
    let (srv, seen) = start_scripted_server(
        asking_bash_rounds("busy.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    // Park a turn on an unanswered permission prompt: the session is busy
    // for as long as nobody answers.
    let sock = srv.sock.clone();
    let mut conn = UnixStream::connect(&sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let (sid, prompt_id) = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break (
                    v["params"]["session_id"].as_str().unwrap().to_string(),
                    v["params"]["event"]["prompt_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    };

    let requests_before = seen.lock().unwrap().len();

    let busy = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"ZZ-refused-message-ZZ"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(busy["error"]["code"], codes::SESSION_BUSY, "{busy}");
    assert_eq!(busy["error"]["data"]["session_id"], sid, "{busy}");
    assert!(
        busy["error"]["data"]["current_turn_id"].is_string(),
        "the refusal must name the turn holding the session: {busy}"
    );

    // Let the parked turn finish.
    let ack = rpc(
        &sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"deny","reason":"test"}}}},"id":99}}"#
        ),
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");

    // The refused message must not have reached the model as part of the
    // turn that was already running.
    let requests: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .skip(requests_before)
        .map(|r| format!("{r:?}"))
        .collect();
    assert!(
        !requests.iter().any(|r| r.contains("ZZ-refused-message-ZZ")),
        "the refused message was fed to the running turn anyway: {requests:?}"
    );

    srv.stop().await;
}

/// Once the parked turn is done the session is idle again — "busy" tracks
/// the turn, it is not a latch.
#[tokio::test]
async fn a_session_accepts_a_new_turn_after_the_previous_one_finishes() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("a"), text_round("b")],
        ask_settings(),
        Duration::ZERO,
    )
    .await;

    let sid = run_turn(&srv.sock, None, "first").await;
    let second = rpc_streaming(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"second"}},"id":2}}"#
        ),
    )
    .await
    .1;
    assert!(second["result"].is_object(), "{second}");

    srv.stop().await;
}

#[tokio::test]
async fn a_client_may_not_borrow_the_turn_id_of_a_turn_the_daemon_starts_itself() {
    let (srv, _seen) = start_scripted_server(
        vec![text_round("x")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    for turn_id in ["goal-1-1-abc", "schedule-job-1", "cron-1"] {
        let resp = rpc(
            &srv.sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"hi","turn_id":"{turn_id}"}},"id":1}}"#
            ),
        )
        .await;
        assert_eq!(resp["error"]["code"], codes::INVALID_PARAMS, "{resp}");
    }
    let resp = rpc(
        &srv.sock,
        r#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"hi","turn_id":"goal-check-t1"},"id":1}"#,
    )
    .await;
    assert!(
        resp["error"].is_null(),
        "an id that only begins like one is the client's: {resp}"
    );
    srv.stop().await;
}

/// Interrupting an idle session is an answer, not an error — the caller
/// wanted it idle and it is.
#[tokio::test]
async fn interrupting_an_idle_session_reports_that_there_was_nothing_to_interrupt() {
    let (srv, _seen) =
        start_scripted_server(vec![text_round("x")], ask_settings(), Duration::ZERO).await;

    let sid = run_turn(&srv.sock, None, "hi").await;
    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":1}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], false, "{resp}");

    srv.stop().await;
}

/// The documented use: a client that hits `SESSION_BUSY` interrupts, then
/// sends. The session survives the interrupt and serves the next turn.
#[tokio::test]
async fn interrupt_frees_a_busy_session_for_the_next_turn() {
    let (srv, _seen) = start_scripted_server(
        vec![
            asking_bash_rounds("interrupt.txt")
                .into_iter()
                .next()
                .unwrap(),
            text_round("after"),
        ],
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
    conn.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"first"},"id":1}"#,
    )
    .await
    .unwrap();
    conn.write_all(b"\n").await.unwrap();

    let sid = {
        let (r, _w) = conn.split();
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed before the prompt arrived");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break v["params"]["session_id"].as_str().unwrap().to_string();
            }
        }
    };

    let resp = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(resp["result"]["interrupted"], true, "{resp}");

    srv.stop().await;
}

/// `session.interrupt` works from the connection running the turn.
///
/// The tab that started a turn is the one with a stop button on it, so this
/// is the case that matters most; it was unreachable for the same reason.
#[tokio::test]
async fn a_turn_can_be_interrupted_from_the_connection_running_it() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("interrupt-same-conn.txt"),
        ask_settings(),
        Duration::from_secs(30),
    )
    .await;

    let interrupted = tokio::time::timeout(Duration::from_secs(10), async {
        let mut conn = UnixStream::connect(&srv.sock).await.unwrap();
        conn.write_all(
            br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"go"},"id":1}"#,
        )
        .await
        .unwrap();
        conn.write_all(b"\n").await.unwrap();

        let (r, mut w) = conn.split();
        let mut br = BufReader::new(r);
        let mut asked = false;
        loop {
            let mut line = String::new();
            let n = br.read_line(&mut line).await.unwrap();
            assert!(n > 0, "connection closed");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();

            // Wait until the turn is definitely in flight, then interrupt it
            // on this same socket.
            if v["params"]["event"]["kind"] == "prompt" && !asked {
                let sid = v["params"]["session_id"].as_str().unwrap();
                w.write_all(format!(
                    r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":2}}"#
                ).as_bytes()).await.unwrap();
                w.write_all(b"\n").await.unwrap();
                asked = true;
            }

            if v["id"] == 2 {
                return v;
            }
        }
    })
    .await
    .expect("session.interrupt was never answered on the connection running the turn");

    assert_eq!(interrupted["result"]["interrupted"], true, "{interrupted}");

    srv.stop().await;
}

/// Start a turn whose Bash call stops at a permission prompt, and return the
/// connection it streams on together with the session and prompt ids. The
/// prompt holds the turn mid-step for as long as the test needs.
async fn turn_held_at_a_prompt(
    sock: &std::path::Path,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
    String,
    String,
) {
    let conn = UnixStream::connect(sock).await.unwrap();
    let (r, mut w) = conn.into_split();
    w.write_all(
        br#"{"jsonrpc":"2.0","method":"session.run_turn","params":{"message":"make a file","turn_id":"t1"},"id":1}"#,
    )
    .await
    .unwrap();
    w.write_all(b"\n").await.unwrap();
    let mut br = BufReader::new(r);
    let (sid, prompt_id) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut line = String::new();
            assert!(
                br.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["params"]["event"]["kind"] == "prompt" {
                break (
                    v["params"]["session_id"].as_str().unwrap().to_string(),
                    v["params"]["event"]["prompt_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    })
    .await
    .expect("the turn never asked");
    (br, w, sid, prompt_id)
}

async fn drain_to_response(
    br: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut frames = Vec::new();
        loop {
            let mut line = String::new();
            assert!(
                br.read_line(&mut line).await.unwrap() > 0,
                "connection closed"
            );
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("id").is_some() {
                return (frames, v);
            }
            frames.push(v);
        }
    })
    .await
    .expect("the turn never finished")
}

#[tokio::test]
async fn a_steered_message_reaches_the_running_turn_after_its_tool_step() {
    let (srv, seen) = start_scripted_server(
        asking_bash_rounds("steered.txt"),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;
    let (mut turn, _w, sid, prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let refused = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"hello?"}},"id":2}}"#
        ),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        codes::SESSION_BUSY,
        "the default stays reject: {refused}"
    );

    let steered = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"also add a README","turn_id":"m1","on_busy":"steer"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(steered["result"]["delivery"], "steer", "{steered}");
    assert_eq!(steered["result"]["into_turn_id"], "t1", "{steered}");

    permit(&srv.sock, &sid, &prompt_id).await;
    let (frames, response) = drain_to_response(&mut turn).await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert_eq!(response["result"]["delivery"], "turn");

    let delivered = events_of_kind(&frames, "user_message_delivered");
    assert_eq!(delivered.len(), 1, "{frames:?}");
    assert_eq!(delivered[0]["message_id"], "m1");
    assert_eq!(delivered[0]["content"], "also add a README");
    let kinds: Vec<String> = frames
        .iter()
        .map(|f| {
            f["params"]["event"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
    assert!(at("tool_result") < at("user_message_delivered"));
    assert!(at("user_message_delivered") < at("turn_complete"));
    assert_eq!(events_of_kind(&frames, "turn_complete").len(), 1);

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "the tool call, then the answer");
    let second = serde_json::to_value(&requests[1].messages).unwrap();
    let rendered: Vec<String> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.to_string())
        .collect();
    let steer_at = rendered
        .iter()
        .position(|m| m.contains("also add a README"))
        .expect("the answer was asked with the message in it");
    let result_at = rendered
        .iter()
        .position(|m| m.contains("tool_result"))
        .expect("and the tool result");
    assert!(result_at < steer_at, "{rendered:?}");

    let transcript = history(&srv.sock, &sid).await.to_string();
    assert!(transcript.contains("also add a README"), "{transcript}");

    srv.stop().await;
}

#[tokio::test]
async fn a_queued_message_runs_as_the_next_turn_once_the_session_is_free() {
    let mut rounds = asking_bash_rounds("queued.txt");
    rounds.push(text_round("second answer"));
    let (srv, seen) =
        start_scripted_server(rounds, ask_settings_no_memory(), Duration::from_secs(30)).await;
    let (mut turn, _w, sid, prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let queued = {
        let sock = srv.sock.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            rpc_streaming(
                &sock,
                &format!(
                    r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"next","turn_id":"t2","on_busy":"queue"}},"id":4}}"#
                ),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !queued.is_finished(),
        "a queued turn waits for the running one"
    );

    permit(&srv.sock, &sid, &prompt_id).await;
    let (_frames, first) = drain_to_response(&mut turn).await;
    assert!(first["result"].is_object(), "{first}");

    let (frames, second) = tokio::time::timeout(Duration::from_secs(20), queued)
        .await
        .expect("the queued turn never ran")
        .unwrap();
    assert_eq!(second["result"]["turn_id"], "t2", "{second}");
    assert_eq!(second["result"]["delivery"], "turn");
    assert_eq!(events_of_kind(&frames, "turn_complete").len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 3);

    srv.stop().await;
}

#[tokio::test]
async fn an_interrupted_turn_hands_back_a_steered_message_it_never_read() {
    let (srv, _seen) = start_scripted_server(
        asking_bash_rounds("returned.txt"),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;
    let (mut turn, _w, sid, _prompt_id) = turn_held_at_a_prompt(&srv.sock).await;

    let steered = rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"wait, stop","turn_id":"m1","on_busy":"steer"}},"id":3}}"#
        ),
    )
    .await;
    assert_eq!(steered["result"]["delivery"], "steer", "{steered}");
    rpc(
        &srv.sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.interrupt","params":{{"session_id":"{sid}"}},"id":5}}"#
        ),
    )
    .await;

    let (frames, _response) = drain_to_response(&mut turn).await;
    let returned = events_of_kind(&frames, "user_messages_returned");
    assert_eq!(returned.len(), 1, "{frames:?}");
    assert_eq!(returned[0]["messages"][0]["message_id"], "m1");
    assert_eq!(returned[0]["messages"][0]["content"], "wait, stop");
    assert!(events_of_kind(&frames, "user_message_delivered").is_empty());

    let transcript = history(&srv.sock, &sid).await.to_string();
    assert!(!transcript.contains("wait, stop"), "{transcript}");

    srv.stop().await;
}
