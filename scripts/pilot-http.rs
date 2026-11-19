//! Loopback HTTP fixture and Grunt launcher for the Docker pilot only.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn respond(mut connection: TcpStream) -> io::Result<()> {
    let timeout = Some(Duration::from_secs(1));
    connection.set_read_timeout(timeout)?;
    connection.set_write_timeout(timeout)?;

    let mut request = BufReader::new(&mut connection).take(8192);
    let mut line = String::new();
    request.read_line(&mut line)?;
    if !line.starts_with("GET / HTTP/1.") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected request",
        ));
    }
    loop {
        line.clear();
        if request.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete headers",
            ));
        }
        if line == "\r\n" {
            break;
        }
    }
    connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")?;
    println!("pilot-http: 200");
    Ok(())
}

fn main() -> io::Result<ExitCode> {
    let listener = TcpListener::bind("127.0.0.1:18080")?;
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            if let Err(error) = connection.and_then(|stream| {
                println!("pilot-http: connection");
                respond(stream)
            }) {
                eprintln!("pilot-http: {error}");
            }
        }
    });

    // The listener is already bound when the Grunt announces readiness.
    let status = Command::new("/horde")
        .args(std::env::args_os().skip(1))
        .status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}
