#![allow(dead_code)]

use std::{
    io::{self, Read},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

pub fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    accept_with_timeout(listener, IO_TIMEOUT)
        .expect("test server timed out waiting for a connection")
}

pub fn accept_with_timeout(
    listener: &TcpListener,
    timeout: Duration,
) -> io::Result<(TcpStream, SocketAddr)> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        match listener.accept() {
            Ok(connection) => break Ok(connection),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "waiting for a connection",
                    ));
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => break Err(error),
        }
    };
    listener.set_nonblocking(false)?;
    let (stream, address) = result?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok((stream, address))
}

pub trait CommandTimeout {
    fn output_timeout(&mut self) -> io::Result<Output>;
}

impl CommandTimeout for Command {
    fn output_timeout(&mut self) -> io::Result<Output> {
        output_with_timeout(self, PROCESS_TIMEOUT)
    }
}

pub fn output_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = out_tx.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err_tx.send(stderr.read_to_end(&mut bytes).map(|_| bytes));
    });
    let deadline = Instant::now() + timeout;
    let timed_out = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("command exceeded {timeout:?}"),
        )
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = out_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| timed_out())??;
    let stderr = err_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| timed_out())??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
