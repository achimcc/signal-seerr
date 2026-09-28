//! Homeserver audit 3, B94: the real `signal-sieb` binary between a client
//! and a stand-in for signal-cli. `addDevice` must never reach signal-cli;
//! `send` must, and what signal-cli writes back must reach the client.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn only_the_bots_methods_reach_signal_cli() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("signal-cli.sock");
    let listen = dir.path().join("sieb.sock");

    // The stand-in for signal-cli: records every line, answers each with a
    // result, and sends one `receive` notification first.
    let l = UnixListener::bind(&upstream).unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let (s, _) = l.accept().unwrap();
        let mut w = s.try_clone().unwrap();
        writeln!(
            w,
            r#"{{"jsonrpc":"2.0","method":"receive","params":{{"account":"+1"}}}}"#
        )
        .unwrap();
        for line in BufReader::new(s).lines() {
            let line = line.unwrap();
            tx.send(line.clone()).unwrap();
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            writeln!(w, r#"{{"jsonrpc":"2.0","id":{},"result":{{}}}}"#, v["id"]).unwrap();
        }
    });

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_signal-sieb"))
        .arg(&listen)
        .arg(&upstream)
        .spawn()
        .unwrap();
    let mut tries = 0;
    let client = loop {
        match UnixStream::connect(&listen) {
            Ok(c) => break c,
            Err(_) if tries < 100 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("sieve never listened: {e}"),
        }
    };
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut w = client.try_clone().unwrap();
    let mut r = BufReader::new(client);
    let mut read = || {
        let mut s = String::new();
        r.read_line(&mut s).unwrap();
        serde_json::from_str::<serde_json::Value>(&s).unwrap()
    };

    // The notification from signal-cli arrives unchanged.
    assert_eq!(read()["method"], "receive");

    writeln!(w, r#"{{"jsonrpc":"2.0","id":"a1","method":"addDevice","params":{{"uri":"sgnl://linkdevice?x"}}}}"#).unwrap();
    let refused = read();
    assert_eq!(refused["id"], "a1");
    assert_eq!(refused["error"]["code"], -32601);

    writeln!(w, r#"{{"jsonrpc":"2.0","id":"s1","method":"send","params":{{"recipient":["x"],"message":"hi"}}}}"#).unwrap();
    assert_eq!(read()["id"], "s1");

    // signal-cli saw the send, and nothing else.
    let seen: Vec<String> = rx
        .recv_timeout(Duration::from_secs(5))
        .into_iter()
        .collect();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].contains(r#""method":"send""#), "{seen:?}");
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "more than the send reached signal-cli"
    );

    child.kill().unwrap();
    let _ = child.wait();
}
