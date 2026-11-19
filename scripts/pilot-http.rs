//! Loopback HTTP fixture and Grunt launcher for the Docker pilot only.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
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

fn serve_fixture() -> io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:18080")?;
    writeln!(io::stderr(), "ready")?;
    for connection in listener.incoming() {
        if let Err(error) = connection.and_then(|stream| {
            println!("pilot-http: connection");
            respond(stream)
        }) {
            println!("pilot-http: {error}");
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().is_some_and(|arg| arg == "--serve-fixture") {
        return serve_fixture();
    }
    let mut fixture = Command::new(std::env::current_exe()?)
        .arg("--serve-fixture")
        .stderr(Stdio::piped())
        .spawn()?;
    let mut ready = String::new();
    BufReader::new(fixture.stderr.take().expect("piped stderr")).read_line(&mut ready)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other(format!("HTTP fixture failed: {ready}")));
    }
    // Replace the launcher so Docker delivers SIGTERM directly to the Grunt.
    Err(Command::new("/horde").args(args).exec())
}
