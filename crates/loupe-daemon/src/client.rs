use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use loupe_proto::{Request, Response, read_frame, write_frame};

/// Blocking client for a running daemon.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
}

impl Client {
    /// Connect to the daemon serving `index_dir`. Fails fast if none is running.
    pub fn connect(index_dir: &Path) -> io::Result<Client> {
        Self::connect_socket(&crate::socket_path(index_dir))
    }

    pub fn connect_socket(socket: &Path) -> io::Result<Client> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        Ok(Client { stream })
    }

    pub fn request(&mut self, request: &Request) -> io::Result<Response> {
        write_frame(&mut self.stream, request)?;
        read_frame(&mut self.stream)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed the connection")
        })
    }

    /// Like [`Client::request`], but turns `Response::Error` into an `Err`.
    pub fn call(&mut self, request: &Request) -> io::Result<Response> {
        match self.request(request)? {
            Response::Error(msg) => Err(io::Error::other(msg)),
            other => Ok(other),
        }
    }

    /// Raw stream, for tests that need to send malformed frames.
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}
