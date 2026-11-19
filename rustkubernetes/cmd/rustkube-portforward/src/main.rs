//! `rustkube-portforward PORT`: connects to `localhost:PORT` and copies
//! stdin → socket and socket → stdout, like the `socat - TCP4:localhost:PORT`
//! that cri-dockerd runs with `nsenter` in a pod's network namespace.
//!
//! The kubelet runs it in a short-lived container that joins the pod
//! sandbox's network namespace (`NetworkMode: container:<sandbox>`), so it
//! reaches listeners bound to the pod's loopback too. Errors go to stderr and
//! end with exit code 1; the kubelet reports them on the port-forward error
//! stream. The message follows containerd's `portForward`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};

fn main() {
    let port: u16 = match std::env::args().nth(1).and_then(|p| p.parse().ok()) {
        Some(p) if p > 0 => p,
        _ => {
            eprintln!("usage: rustkube-portforward PORT");
            std::process::exit(2);
        }
    };
    // IPv4 loopback first, then IPv6.
    let conn = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(c) => c,
        Err(e4) => match TcpStream::connect(("::1", port)) {
            Ok(c) => c,
            Err(e6) => {
                eprint!(
                    "failed to connect to localhost:{port} inside the pod network namespace, IPv4: dial tcp4 127.0.0.1:{port}: connect: {} IPv6 dial tcp6 [::1]:{port}: connect: {} ",
                    describe(&e4),
                    describe(&e6)
                );
                std::process::exit(1);
            }
        },
    };
    let mut to_sock = conn.try_clone().expect("clone socket");
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_sock.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        // The client finished sending: half-close, keep reading replies.
        let _ = to_sock.shutdown(Shutdown::Write);
    });
    let mut from_sock = conn;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 32 * 1024];
    loop {
        match from_sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprint!("error reading from localhost:{port}: {}", describe(&e));
                std::process::exit(1);
            }
        }
    }
}

/// Go-style text of a socket error ("connection refused").
fn describe(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".into(),
        std::io::ErrorKind::ConnectionReset => "connection reset by peer".into(),
        _ => {
            let s = e.to_string();
            let s = s.split(" (os error").next().unwrap_or(&s).to_string();
            s.to_lowercase()
        }
    }
}
