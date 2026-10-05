// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Bounded JSON hardware frames over QEMU's private virtio-serial channel.
//!
//! Each UTF-8 message is COBS encoded and terminated by a zero byte.
//! Decoded messages are bounded to 64 KiB.
//! The guest starts each session with {"version":1}; the launcher acknowledges
//! that message before either side sends driver frames. A new greeting resets
//! the connection generation even when QEMU keeps the host channel open.

use darkbio_cobs as cobs;
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use super::local::{self, Stream};

/// Maximum serialized hardware message length in bytes.
pub(crate) const MAX_FRAME: usize = 64 * 1024;
/// Maximum encoded frame length before its zero delimiter.
const MAX_ENCODED: usize = cobs::encode_buffer(MAX_FRAME);
/// Session greeting and acknowledgement, sent before driver traffic.
pub(crate) const HELLO: &str = r#"{"version":1}"#;
/// Name exposed inside the guest by the dedicated virtio-serial port.
pub(crate) const PORT_NAME: &str = "bio.dark.hw.v1";
/// Maximum duration of one complete frame write.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// One launch's private QEMU channel, retained until its hardware worker exits.
pub(crate) struct Endpoint {
    /// Filesystem socket or fully qualified Windows pipe name.
    path: PathBuf,
    /// Private directory prevents access before QEMU creates its socket.
    #[cfg(unix)]
    _directory: tempfile::TempDir,
}

impl Endpoint {
    /// Reserve an isolated channel name before spawning QEMU.
    pub(crate) fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::Builder::new()
                .prefix("ark-hw-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")?;
            local::track_socket(directory.path().join("hw.sock"), true)?;
            Ok(Self {
                path: directory.path().join("hw.sock"),
                _directory: directory,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                path: PathBuf::from(format!(r"\\.\pipe\ark-hw-{}", local::identity())),
            })
        }
    }

    /// Describe the QEMU backend while leaving the guest console on its own device.
    pub(crate) fn chardev(&self) -> String {
        #[cfg(unix)]
        {
            format!(
                "socket,id=hw,path={},server=on,wait=off",
                self.path.display()
            )
        }
        #[cfg(windows)]
        {
            format!(
                "pipe,id=hw,path={}",
                self.path
                    .to_str()
                    .unwrap()
                    .strip_prefix(r"\\.\pipe\")
                    .unwrap()
            )
        }
    }

    /// Open QEMU's endpoint without exposing a TCP fallback.
    pub(crate) fn connect(&self, pid: u32, timeout: Duration) -> io::Result<Channel> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = pid;
            let stream = Stream::connect_path(&self.path, timeout)?;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Channel::new(stream))
        }
        #[cfg(windows)]
        {
            Ok(Channel::new(Stream::connect_qemu(
                &self.path, pid, timeout,
            )?))
        }
    }

    /// Locate a fake QEMU backend in transport tests.
    #[cfg(test)]
    pub(crate) fn fixture() -> (Self, local::Server) {
        let name = format!("t-{}", local::identity());
        let server = local::Server::bind(&name).unwrap();
        let mut endpoint = Self::new().unwrap();
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
            return Err(if self.encoded.len() > MAX_ENCODED {
                io::Error::new(io::ErrorKind::InvalidData, "hardware frame too large")
            } else {
                io::ErrorKind::UnexpectedEof.into()
            });
        }

        // Decode only a complete frame, leaving any following frames in the reader
        self.encoded.pop();
        let mut body = vec![0; cobs::decode_buffer(self.encoded.len())];
        let length = cobs::decode(&self.encoded, &mut body)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.encoded.clear();
        if length == 0 || length > MAX_FRAME {
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
        let endpoint = Endpoint::new().unwrap();
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
        let client = endpoint
            .connect(std::process::id(), Duration::from_secs(1))
            .unwrap();
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
            (b"\x0e{\"version\":1}\0".as_slice(), HELLO),
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
            assert_eq!(client.read(Duration::from_secs(1)).unwrap(), HELLO);
            assert_eq!(client.read(Duration::from_secs(1)).unwrap(), "{}");
        }
    }

    /// The payload limit allows COBS overhead and bounds unterminated input.
    #[test]
    fn test_hardware_frame_limits() {
        let (mut client, mut peer, _endpoint, _server) = pair();
        let sender = std::thread::spawn(move || peer.send(&"x".repeat(65536)).unwrap());
        assert_eq!(
            client.read(Duration::from_secs(3)).unwrap(),
            "x".repeat(65536)
        );
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
