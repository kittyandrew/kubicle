//! Frames the server's byte stream into whole responses, each decoded once. A line ending in `{n}` CRLF is
//! followed by `n` raw bytes, then the rest of the response. The framer finds line ends with memchr and steps
//! over literals by length, so a 5 MB body costs one scan of the line announcing it, not repeated parses. It
//! matches imap-codec's `Fragmentizer`, which uses a byte queue; only a whole response reaches imap-codec.

use std::{io, time::Duration};

use bytes::{Bytes, BytesMut};
use imap_codec::{GreetingCodec, ResponseCodec, decode::Decoder, imap_types::response::Greeting, imap_types::response::Response};
use tokio::{io::AsyncRead, io::AsyncReadExt, io::AsyncWrite, io::AsyncWriteExt};
use tracing::warn;

use crate::Error;

/// The largest response accepted, literals included: above Mailu's default 50 MB message size limit.
const MAX_RESPONSE: usize = 64 << 20;
const READ_CHUNK: usize = 64 << 10;
/// The most buffer kept once its response is split off. A whole message grows it to the message's size, which would
/// otherwise stay allocated for the connection's life.
const KEPT: usize = 1 << 20;
/// How long the server may send nothing while a reply is due. A link that died without a reset (a sleeping NAT,
/// a pulled cable) would otherwise keep the engine waiting forever; this error makes it reconnect instead. It
/// bounds the gap between reads, not the whole reply, so a 50 MB FETCH that keeps arriving is never cut, and
/// 60 s leaves room for a SEARCH or EXPUNGE over a large folder, which sends nothing until it is done.
const SILENCE: Duration = Duration::from_secs(60);

pub struct Stream<S> {
    io: S,
    /// Bytes read and not yet handed out; the response being framed starts at its front.
    buf: BytesMut,
    /// Where framing resumes: the end of the last line or literal of the current response that was found.
    cursor: usize,
    /// The response last handed out, split off `buf`, so a part of it can be kept without a copy (`part_of`).
    frame: Bytes,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S> {
    pub fn new(io: S) -> Self { Self { io, buf: BytesMut::with_capacity(READ_CHUNK), cursor: 0, frame: Bytes::new() } }

    /// The transport, for the TLS upgrade after STARTTLS. Fails when the server sent bytes after the
    /// STARTTLS answer: those would have been read before the encryption that is meant to protect them.
    pub fn into_inner(self) -> Result<S, Error> {
        if self.buf.is_empty() { Ok(self.io) } else { Err(Error::Malformed("data after STARTTLS")) }
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), Error> { Ok(self.io.write_all(bytes).await?) }

    pub async fn flush(&mut self) -> Result<(), Error> { Ok(self.io.flush().await?) }

    pub async fn greeting(&mut self) -> Result<Greeting<'_>, Error> {
        self.frame(Some(SILENCE)).await?;
        if let Ok(([], greeting)) = GreetingCodec::new().decode(&self.frame) { Ok(greeting) } else { Err(Error::Malformed("greeting")) }
    }

    /// The next response and the bytes it was decoded from, failing after `SILENCE` without a byte.
    /// `Error::Unparsed` is recoverable: an untagged response imap-codec does not know, whose end the framing still
    /// found, so the caller can go on with the next one.
    pub async fn response(&mut self) -> Result<(Response<'_>, &Bytes), Error> { self.decode(Some(SILENCE)).await }

    /// The bytes of the response last read, also when it did not decode (`Error::Unparsed`).
    pub fn last_frame(&self) -> &Bytes { &self.frame }

    /// The next response during IDLE, which may take any time. Cancel-safe: dropping the future loses no bytes,
    /// so it can race other events.
    pub async fn idle_response(&mut self) -> Result<Response<'_>, Error> { Ok(self.decode(None).await?.0) }

    async fn decode(&mut self, silence: Option<Duration>) -> Result<(Response<'_>, &Bytes), Error> {
        self.frame(silence).await?;
        let message = &self.frame;
        match ResponseCodec::new().decode(message) {
            Ok(([], response)) => Ok((response, message)),
            _ if message.starts_with(b"* ") => {
                warn!(bytes = message.len(), "skipped an IMAP response that does not parse");
                Err(Error::Unparsed)
            }
            _ => Err(Error::Malformed("response")),
        }
    }

    /// Frames the next complete response into `frame`.
    async fn frame(&mut self, silence: Option<Duration>) -> Result<(), Error> {
        self.frame = Bytes::new(); // else it shares the buffer, and each `read` below would reallocate it
        loop {
            let Some(newline) = memchr::memchr(b'\n', &self.buf[self.cursor..]) else {
                if self.buf.len() > MAX_RESPONSE {
                    return Err(Error::TooLarge);
                }
                self.read(READ_CHUNK, silence).await?;
                continue;
            };
            let line_end = self.cursor + newline + 1;
            let Some(length) = literal_length(&self.buf[self.cursor..line_end]) else {
                let outgrown = self.buf.capacity() > KEPT;
                self.frame = self.buf.split_to(line_end).freeze();
                if outgrown {
                    self.buf = BytesMut::from(&self.buf[..]); // the rest leaves the big allocation, freed with the frame and its parts
                }
                self.cursor = 0;
                return Ok(());
            };
            let end = line_end + length;
            if end > MAX_RESPONSE {
                return Err(Error::TooLarge);
            }
            while self.buf.len() < end {
                // With room for what follows the literal: a buffer the literal fills exactly doubles on the next read.
                self.read(end - self.buf.len() + READ_CHUNK, silence).await?;
            }
            self.cursor = end;
        }
    }

    /// Reads at least one byte, reserving room for `want` more. `reserve` moves what is left to the front of a
    /// buffer no frame shares any more, else into a new one.
    async fn read(&mut self, want: usize, silence: Option<Duration>) -> Result<(), Error> {
        self.buf.reserve(want.max(READ_CHUNK));
        let read = self.io.read_buf(&mut self.buf);
        let read = match silence {
            Some(limit) => tokio::time::timeout(limit, read).await.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?,
            None => read.await,
        };
        if read? == 0 {
            return Err(Error::Closed);
        }
        Ok(())
    }
}

/// `part`, a slice of `frame`, as bytes sharing its allocation, so a whole message is never held twice. A part the
/// decoder built itself (a quoted string with escapes) is not in the frame and is copied.
pub fn part_of(frame: &Bytes, part: &[u8]) -> Bytes {
    let (inside, within) = (frame.as_ptr_range(), part.as_ptr_range());
    if inside.start <= within.start && within.end <= inside.end { frame.slice_ref(part) } else { Bytes::copy_from_slice(part) }
}

/// The length announced by a line ending in `{n}` CRLF, or `{n+}` CRLF in the non-synchronizing form.
fn literal_length(line: &[u8]) -> Option<usize> {
    let line = line.strip_suffix(b"\n")?;
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = line.strip_suffix(b"}")?;
    let line = line.strip_suffix(b"+").unwrap_or(line);
    let open = line.iter().rposition(|&b| b == b'{')?;
    let digits = &line[open + 1..];
    if digits.is_empty() || digits.len() > 10 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0usize, |n, &d| n.checked_mul(10)?.checked_add(usize::from(d - b'0')))
}
