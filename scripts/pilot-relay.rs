//! Single-connection TCP relay for silencing the Docker pilot's control stream.
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

fn forward(source: &mut TcpStream, destination: &mut TcpStream) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    match source.read(&mut buffer) {
        Ok(0) => Ok(false),
        Ok(count) => {
            destination.write_all(&buffer[..count])?;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn main() -> io::Result<()> {
    let [_, listen, grunt]: [String; 3] = std::env::args()
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| io::Error::other("expected a listen address and a Grunt address"))?;
    let listener = TcpListener::bind(&listen)?;
    println!("pilot-relay: ready");
    let (mut controller, _) = listener.accept()?;
    drop(listener);
    let mut grunt = TcpStream::connect(grunt)?;
    for stream in [&controller, &grunt] {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_millis(15)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    }
    while !Path::new("/identity/silent").exists() {
        if !forward(&mut controller, &mut grunt)? || !forward(&mut grunt, &mut controller)? {
            return Err(io::Error::other(
                "control connection closed before the silence",
            ));
        }
    }
    println!("pilot-relay: traffic paused");
    // Keep ownership of both sockets until Docker cleans up the relay.
    loop {
        std::thread::park();
    }
}
