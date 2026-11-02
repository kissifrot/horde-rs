//! Minimal HTTP/1.1 loopback server for the benchmark harness.
use std::{
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    thread,
};

const WORKERS: usize = 128;
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";

fn serve(stream: TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut stream = BufReader::new(stream);
    let mut line = Vec::with_capacity(1024);
    loop {
        let mut close = false;
        loop {
            line.clear();
            if stream.read_until(b'\n', &mut line)? == 0 {
                return Ok(());
            }
            if line == b"\r\n" {
                break;
            }
            if let Some(index) = line.iter().position(|&byte| byte == b':') {
                let (name, value) = (&line[..index], &line[index + 1..]);
                if name.eq_ignore_ascii_case(b"connection") {
                    close |= value
                        .split(|&byte| byte == b',' || byte.is_ascii_whitespace())
                        .any(|part| part.eq_ignore_ascii_case(b"close"));
                }
            }
        }
        stream.get_mut().write_all(RESPONSE)?;
        if close {
            return Ok(());
        }
    }
}

fn main() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    for _ in 0..WORKERS {
        let listener = listener.try_clone()?;
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let _ = serve(stream);
                    }
                    Err(error) => eprintln!("accept failed: {error}"),
                }
            }
        });
    }
    println!("{port}");
    io::stdout().flush()?;
    loop {
        thread::park();
    }
}
