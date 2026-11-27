//! The q listener: `Poll::serve_q` accepts q peers on a port, evaluates what
//! they send synchronously, and dispatches what they push asynchronously into
//! the runtime's environment. This is what turns a runtime into an RDB that a q
//! publisher can write into.
//!
//! Driven by raw TCP peers on helper threads. A `QConnection` on the runtime
//! thread could not be the peer: its `execute` blocks for a response that only
//! the poll can produce, and the poll runs on the same thread.

mod support;

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use rayforce::{q, Poll, Runtime};
use support::pump_until;
use support::wire::{
    char_vec, dict, frame, list, long_atom, long_vec, read_frame, sym_atom, sym_vec, ASYNC,
    RESPONSE, SYNC,
};

/// A port for a listener. `q_serve` refuses 0, so the number has to be
/// chosen here: a counter over a range below every ephemeral range, which no
/// two calls in this process can share, and a bind to skip a port some other
/// process on the box already holds.
fn free_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    const BASE: u16 = 21000;
    loop {
        let port = BASE + NEXT.fetch_add(1, Ordering::Relaxed);
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Connect and log in anonymously on a helper thread, handing the socket back
/// once the listener has answered the handshake. On a thread because that
/// answer only comes once the poll has run, which the test does meanwhile.
fn connect(port: u16) -> Receiver<TcpStream> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("the port is served");
        // Anonymous login: an empty name, capability byte 3, NUL.
        sock.write_all(&[3, 0]).unwrap();
        let mut cap = [0u8; 1];
        std::io::Read::read_exact(&mut sock, &mut cap).expect("the handshake is answered");
        let _ = tx.send(sock);
    });
    rx
}

/// Send one SYNC frame and collect its RESPONSE on a helper thread, for the
/// same reason as `connect`.
fn send_sync(sock: &TcpStream, body: Vec<u8>) -> Receiver<(u8, Vec<u8>)> {
    let (tx, rx) = mpsc::channel();
    let mut sock = sock.try_clone().unwrap();
    thread::spawn(move || {
        sock.write_all(&frame(SYNC, &body)).unwrap();
        if let Some(reply) = read_frame(&mut sock) {
            let _ = tx.send(reply);
        }
    });
    rx
}

#[test]
fn a_sync_string_is_evaluated_and_answered() {
    Runtime::scope(|_rt| {
        let poll = Poll::install()?;
        let port = free_port();
        let _listener = poll.serve_q(port)?;

        let peer = connect(port);
        let sock = pump_until(&poll, || peer.try_recv().ok()).expect("logged in");
        let reply = send_sync(&sock, char_vec("(+ 1 2)"));
        let (msgtype, body) = pump_until(&poll, || reply.try_recv().ok()).expect("answered");

        assert_eq!(msgtype, RESPONSE);
        assert_eq!(body, long_atom(3));
        Ok(())
    })
    .unwrap();
}

/// The contract an RDB is built on: a q publisher pushes `(upd; dict)` and a
/// function defined in Rayfall receives the dict, keys and tables intact.
#[test]
fn an_async_push_reaches_a_rayfall_function() {
    Runtime::scope(|rt| {
        rt.eval("(set upd (fn [p] (set got p)))")?;
        let poll = Poll::install()?;
        let port = free_port();
        let _listener = poll.serve_q(port)?;

        let peer = connect(port);
        let mut sock = pump_until(&poll, || peer.try_recv().ok()).expect("logged in");
        let payload = dict(sym_vec(&["trade"]), list(&[long_vec(&[10, 11, 12])]));
        sock.write_all(&frame(ASYNC, &list(&[sym_atom("upd"), payload])))
            .unwrap();

        let got = pump_until(&poll, || rayforce::get_global("got").ok()).expect("upd ran");
        assert!(got.is_dict(), "got {}", got.format());
        assert_eq!(got.dict_keys()?.get(0)?.as_sym()?, "trade");
        assert_eq!(got.dict_values()?.get(0)?.as_slice::<i64>()?, &[10, 11, 12]);
        Ok(())
    })
    .unwrap();
}

/// The listener's connections copy the poll's restricted flag when it
/// accepts them, and a restricted q connection refuses a bare string of
/// source text (rayforce-q's README, "For an untrusted Q peer"). The next
/// test sends the same text in a form it does not refuse.
#[test]
fn a_restricted_listener_refuses_source_text() {
    Runtime::scope(|_rt| {
        let poll = Poll::install()?;
        poll.set_restricted(true)?;
        let port = free_port();
        let _listener = poll.serve_q(port)?;

        let peer = connect(port);
        let sock = pump_until(&poll, || peer.try_recv().ok()).expect("logged in");
        let reply = send_sync(&sock, char_vec("(+ 1 2)"));
        let (msgtype, body) = pump_until(&poll, || reply.try_recv().ok()).expect("answered");

        assert_eq!(msgtype, RESPONSE);
        let refused = q::decode_response(&frame(RESPONSE, &body))
            .expect_err("an error, not a value")
            .to_string();
        assert!(refused.contains("access"), "refused with access: {refused}");
        Ok(())
    })
    .unwrap();
}

/// rayforce-q registers `.q.dispatch_sync` unrestricted (its
/// embed/rayforce_q.c, `q_env_register`), so a restricted peer can send
/// source text as `(.q.dispatch_sync; "text")` on a frame no hook covers.
/// With no sync hook the text runs under the restricted rule, so a read
/// answers and a `set` is refused. This pins today's behaviour. If the read
/// starts failing with `access`, rayforce-q has closed the gap: flip that
/// assertion and correct `Poll::set_restricted` and the IPC guide.
#[test]
fn a_restricted_peer_runs_source_text_through_dispatch_sync() {
    Runtime::scope(|rt| {
        let poll = Poll::install()?;
        poll.set_restricted(true)?;
        let port = free_port();
        let _listener = poll.serve_q(port)?;

        let peer = connect(port);
        let sock = pump_until(&poll, || peer.try_recv().ok()).expect("logged in");
        let dispatch = |text: &str| {
            let request = list(&[sym_atom(".q.dispatch_sync"), char_vec(text)]);
            let reply = send_sync(&sock, request);
            let (_, body) = pump_until(&poll, || reply.try_recv().ok()).expect("answered");
            q::decode_response(&frame(RESPONSE, &body))
        };

        assert_eq!(dispatch("(+ 40 2)")?.as_i64()?, 42);
        let refused = dispatch("(set zz 1)")
            .expect_err("an error, not a value")
            .to_string();
        assert!(refused.contains("access"), "refused with access: {refused}");
        assert!(rt.eval("zz").is_err(), "the refused set bound nothing");
        Ok(())
    })
    .unwrap();
}

#[test]
fn port_zero_is_refused() {
    Runtime::scope(|_rt| {
        let poll = Poll::install()?;
        assert!(poll.serve_q(0).is_err());
        Ok(())
    })
    .unwrap();
}
