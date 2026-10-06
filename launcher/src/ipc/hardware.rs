// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded JSON hardware frames over QEMU's private virtio-serial channel.
//!
//! Each UTF-8 message is COBS encoded and terminated by a zero byte.
//! Decoded messages are bounded to 64 KiB.
//! The guest starts each session with a version 1 greeting and a fresh nonce.
//! The launcher echoes that message before either side sends driver frames.
//! A leading zero separates each greeting from stale partial frames.

use darkbio_cobs as cobs;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use super::local::{self, Stream};

/// Maximum serialized hardware message length in bytes.
pub(crate) const MAX_FRAME: usize = 64 * 1024;
/// Maximum encoded frame length before its zero delimiter.
const MAX_ENCODED: usize = cobs::encode_buffer(MAX_FRAME);
/// Sample greeting used by native peers in protocol tests.
#[cfg(test)]
pub(crate) const HELLO: &str = r#"{"version":1,"session":"0123456789abcdef0123456789abcdef"}"#;
/// Name exposed inside the guest by the dedicated virtio-serial port.
pub(crate) const PORT_NAME: &str = "bio.dark.hw.v1";
/// Maximum duration of one complete frame write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// A frame size violation that requires closing the channel rather than resynchronizing.
#[derive(Debug)]
pub(crate) struct FrameTooLarge;

impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("hardware frame too large")
    }
}

impl std::error::Error for FrameTooLarge {}

/// One private QEMU hardware or monitor channel, retained until its worker exits.
pub(crate) struct Endpoint {
    /// QEMU character backend identifier.
    id: &'static str,
    /// Filesystem socket or fully qualified Windows pipe name.
    path: PathBuf,
    /// Private directory prevents access before QEMU creates its socket.
    #[cfg(unix)]
    _directory: tempfile::TempDir,
}

impl Endpoint {
    /// Reserve an isolated channel name before spawning QEMU.
    pub(crate) fn new(id: &'static str) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::Builder::new()
                .prefix("ark-hw-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")?;
            local::track_socket(directory.path().join("hw.sock"), true)?;
            Ok(Self {
                id,
                path: directory.path().join("hw.sock"),
                _directory: directory,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                id,
                path: PathBuf::from(format!(r"\\.\pipe\ark-hw-{}", local::identity())),
            })
        }
    }

    /// Describe the QEMU backend while leaving the guest console on its own device.
    pub(crate) fn chardev(&self) -> String {
        #[cfg(unix)]
        {
            format!(
                "socket,id={},path={},server=on,wait=off",
                self.id,
                self.path.display()
            )
        }
        #[cfg(windows)]
        {
            format!(
                "pipe,id={},path={}",
                self.id,
                self.path
                    .to_str()
                    .unwrap()
                    .strip_prefix(r"\\.\pipe\")
                    .unwrap()
            )
        }
    }

    /// Open QEMU's endpoint without exposing a TCP fallback.
    pub(crate) fn connect(&self, pid: u32, timeout: Duration) -> io::Result<Stream> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = pid;
            let stream = Stream::connect_path(&self.path, timeout)?;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            Ok(stream)
        }
        #[cfg(windows)]
        {
            Stream::connect_qemu(&self.path, pid, timeout)
        }
    }

    /// Locate a fake QEMU backend in transport tests.
    #[cfg(test)]
    pub(crate) fn fixture() -> (Self, local::Server) {
        let name = format!("t-{}", local::identity());
        let server = local::Server::bind(&name).unwrap();
        let mut endpoint = Self::new("hw").unwrap();
        endpoint.path = local::address(&name).unwrap();
        (endpoint, server)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        #[cfg(unix)]
        local::remove_socket(&self._directory.path().join("hw.sock"));
    }
}

/// One duplex connection with a decoder retained across idle read timeouts.
pub(crate) struct Channel {
    /// Bounded native stream, owned by this worker alone.
    stream: BufReader<Stream>,
    /// Partial encoded frame retained across idle read timeouts.
    encoded: Vec<u8>,
}

impl Channel {
    /// Wrap a native connection with bounded message framing.
    pub(crate) fn new(stream: Stream) -> Self {
        Self {
            stream: BufReader::new(stream),
            encoded: Vec::new(),
        }
    }

    /// Read one frame, retaining partial bytes if this poll reaches its deadline.
    pub(crate) fn read(&mut self, timeout: Duration) -> io::Result<String> {
        self.stream.get_mut().set_read_timeout(timeout);
        // The extra byte holds the delimiter or proves the encoded frame is oversized
        let remaining = MAX_ENCODED + 1 - self.encoded.len();
        self.stream
            .by_ref()
            .take(remaining as u64)
            .read_until(0, &mut self.encoded)?;
        if self.encoded.last() != Some(&0) {
            let oversized = self.encoded.len() > MAX_ENCODED;
            self.encoded.clear();
            return Err(if oversized {
                io::Error::new(io::ErrorKind::InvalidData, FrameTooLarge)
            } else {
                io::ErrorKind::UnexpectedEof.into()
            });
        }

        // Decode only a complete frame, leaving any following frames in the reader
        self.encoded.pop();
        let mut body = vec![0; cobs::decode_buffer(self.encoded.len())];
        let length = cobs::decode(&self.encoded, &mut body);
        self.encoded.clear();
        let length = length.map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if length > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, FrameTooLarge));
        }
        if length == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid hardware frame length",
            ));
        }
        body.truncate(length);
        String::from_utf8(body).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    /// Deliver one complete frame within a single write deadline.
    pub(crate) fn send(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() || text.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid hardware frame length",
            ));
        }
        let mut encoded = vec![0; cobs::encode_buffer(text.len()) + 1];
        let length = cobs::encode(text.as_bytes(), &mut encoded)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        encoded[length] = 0;
        let stream = self.stream.get_mut();
        stream.set_write_timeout(WRITE_TIMEOUT);
        stream.write_all(&encoded[..=length])
    }

    /// Terminate stale partial input before acknowledging a fresh session.
    pub(crate) fn synchronize(&mut self, greeting: &str) -> io::Result<()> {
        self.stream.get_mut().set_write_timeout(WRITE_TIMEOUT);
        self.stream.get_mut().write_all(&[0])?;
        self.send(greeting)
    }

    /// Borrow the transport to exercise fragmentation and framing errors.
    #[cfg(test)]
    pub(crate) fn stream(&mut self) -> &mut Stream {
        self.stream.get_mut()
    }
}

/// Framing vectors shared with the guest protocol and incomplete native reads.
#[cfg(test)]
mod tests {
    use super::*;

    /// The channel directory is private before QEMU starts and removed on drop.
    #[cfg(unix)]
    #[test]
    fn test_endpoint_directory_permissions_and_cleanup() {
        use std::os::unix::fs::PermissionsExt as _;
        let endpoint = Endpoint::new("hw").unwrap();
        let directory = endpoint.path.parent().unwrap().to_path_buf();
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(endpoint);
        assert!(!directory.exists());
    }

    /// Connect two native peers using the production endpoint and access checks.
    fn pair() -> (Channel, Channel, Endpoint, local::Server) {
        let (endpoint, server) = Endpoint::fixture();
        let client = Channel::new(
            endpoint
                .connect(std::process::id(), Duration::from_secs(1))
                .unwrap(),
        );
        let peer = Channel::new(
            server
                .accept_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
        );
        (client, peer, endpoint, server)
    }

    /// The launcher accepts the same COBS and UTF-8 vectors as the guest.
    #[test]
    fn test_hardware_frame_vectors() {
        for (wire, text) in [
            (b"\x0e{\"version\":1}\0".as_slice(), r#"{"version":1}"#),
            (b"\x05\"\xc3\xa9\"\0".as_slice(), "\"é\""),
            (b"\x03ab\x03cd\0".as_slice(), "ab\0cd"),
        ] {
            let (mut client, mut peer, _endpoint, _server) = pair();
            peer.stream().write_all(wire).unwrap();
            assert_eq!(client.read(Duration::from_secs(1)).unwrap(), text);
            client.send(text).unwrap();
            let mut encoded = vec![0; wire.len()];
            peer.stream().read_exact(&mut encoded).unwrap();
            assert_eq!(encoded, wire);
        }
        for wire in [b"\0".as_slice(), b"\x01\0", b"\x03A\0", b"\x02\xff\0"] {
            let (mut client, mut peer, _endpoint, _server) = pair();
            peer.stream().write_all(wire).unwrap();
            assert_eq!(
                client.read(Duration::from_secs(1)).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            peer.send("{}").unwrap();
            assert_eq!(client.read(Duration::from_secs(1)).unwrap(), "{}");
        }
        for wire in [b"".as_slice(), b"\x0e{\"version\":1}", b"\x03A"] {
            let (mut client, mut peer, _endpoint, _server) = pair();
            peer.stream().write_all(wire).unwrap();
            drop(peer);
            assert_eq!(
                client.read(Duration::from_secs(1)).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
    }

    /// Every interrupted frame resumes without losing or replaying bytes.
    #[test]
    fn test_partial_frames_survive_idle_polls() {
        let wire = b"\x0e{\"version\":1}\0";
        for split in 1..wire.len() {
            let (mut client, mut peer, _endpoint, _server) = pair();
            peer.stream().write_all(&wire[..split]).unwrap();
            assert_eq!(
                client.read(Duration::from_millis(10)).unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            peer.stream().write_all(&wire[split..]).unwrap();
            peer.send("{}").unwrap();
            assert_eq!(
                client.read(Duration::from_secs(1)).unwrap(),
                r#"{"version":1}"#
            );
            assert_eq!(client.read(Duration::from_secs(1)).unwrap(), "{}");
        }
    }

    /// Maximum frames cross both ways, with COBS overhead and malformed input bounded.
    #[test]
    fn test_hardware_frame_limits() {
        let (mut client, mut peer, _endpoint, _server) = pair();
        let sender = std::thread::spawn(move || {
            peer.send(&"x".repeat(65536)).unwrap();
            assert_eq!(
                peer.read(Duration::from_secs(3)).unwrap(),
                "y".repeat(65536)
            );
        });
        assert_eq!(
            client.read(Duration::from_secs(3)).unwrap(),
            "x".repeat(65536)
        );
        client.send(&"y".repeat(65536)).unwrap();
        sender.join().unwrap();
        assert_eq!(
            client.send(&"x".repeat(65537)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        // Empty COBS blocks encode zero bytes, so decoded overflow fits the wire bound
        for wire in [[vec![1; 65538], vec![0]].concat(), vec![1; 65797]] {
            let (mut client, mut peer, _endpoint, _server) = pair();
            let sender = std::thread::spawn(move || peer.stream().write_all(&wire).unwrap());
            assert_eq!(
                client.read(Duration::from_secs(3)).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            sender.join().unwrap();
        }
    }

    /// A pipe with the expected name is insufficient unless QEMU owns it.
    #[cfg(windows)]
    #[test]
    fn test_hardware_pipe_rejects_wrong_process() {
        let (endpoint, _server) = Endpoint::fixture();
        assert!(matches!(
            endpoint.connect(0, Duration::from_secs(1)),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied
        ));
    }
}
