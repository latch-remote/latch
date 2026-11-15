//! Small bounded HTTP/1.1 and RFC 6455 carrier. No extensions or compression.
use crate::{crypto::random, invalid, wire::RECORD_MAX, Failure};
use base64::{engine::general_purpose::STANDARD, Engine};
use sha1::{Digest, Sha1};
use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

pub(crate) const HTTP_ALPN: &[u8] = b"http/1.1";
pub const WEBSOCKET_PATH: &str = "/relay";
const HTTP_MAX: usize = 8192;
const MESSAGE_MAX: usize = RECORD_MAX + 4;
const DECOY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 3\r\nConnection: close\r\n\r\nOK\n";

#[derive(Default)]
pub(crate) struct Headers(Vec<u8>);
impl Headers {
    pub fn read_response(&mut self, reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
        let result = self.read(reader);
        if self.0.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'<') {
            return Err(Failure::CaptivePortal.error());
        }
        result
    }
    // Read exactly through the header terminator, leaving tunneled bytes untouched.
    pub fn read(&mut self, reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
        for _ in 0..HTTP_MAX {
            let mut byte = [0];
            match reader.read(&mut byte) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => self.0.push(byte[0]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) => return Err(e),
            }
            if self.0.ends_with(b"\r\n\r\n") {
                return Ok(Some(std::mem::take(&mut self.0)));
            }
            if self.0.len() == HTTP_MAX {
                return Err(invalid("HTTP headers exceed bound"));
            }
        }
        Ok(None)
    }
}
fn header<'a>(headers: &'a [httparse::Header<'a>], name: &str) -> Option<&'a str> {
    let mut matches = headers.iter().filter(|h| h.name.eq_ignore_ascii_case(name));
    let value = std::str::from_utf8(matches.next()?.value).ok()?;
    if matches.next().is_some() {
        return None;
    }
    Some(value.trim())
}
fn token(headers: &[httparse::Header<'_>], name: &str, expected: &str) -> bool {
    header(headers, name).is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case(expected))
    })
}
fn accept_key(key: &str) -> String {
    let mut hash = Sha1::new();
    hash.update(key.as_bytes());
    hash.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(hash.finalize())
}
fn server_upgrade(bytes: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut request = httparse::Request::new(&mut headers);
    if !request
        .parse(bytes)
        .map_err(|_| invalid("invalid HTTP request"))?
        .is_complete()
    {
        return Err(invalid("incomplete HTTP request"));
    }
    let key = header(request.headers, "Sec-WebSocket-Key").unwrap_or("");
    let valid_key = STANDARD.decode(key).is_ok_and(|key| key.len() == 16);
    if request.method != Some("GET")
        || request.path != Some(WEBSOCKET_PATH)
        || request.version != Some(1)
        || !token(request.headers, "Upgrade", "websocket")
        || !token(request.headers, "Connection", "upgrade")
        || header(request.headers, "Sec-WebSocket-Version") != Some("13")
        || !valid_key
        || header(request.headers, "Host").is_none_or(str::is_empty)
        || request
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("Transfer-Encoding"))
        || request
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("Content-Length") && h.value != b"0")
    {
        return Ok(None);
    }
    Ok(Some(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n", accept_key(key)).into_bytes()))
}
pub(crate) fn response_status(bytes: &[u8]) -> io::Result<(u16, bool)> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    if !response
        .parse(bytes)
        .map_err(|_| Failure::CaptivePortal.error())?
        .is_complete()
    {
        return Err(Failure::CaptivePortal.error());
    }
    let status = response
        .code
        .ok_or_else(|| Failure::CaptivePortal.error())?;
    let portal = (300..400).contains(&status)
        || header(response.headers, "Content-Type")
            .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
    Ok((status, portal))
}
fn client_upgrade(bytes: &[u8], accept: &str) -> io::Result<()> {
    let (status, portal) = response_status(bytes)?;
    if portal || status == 200 {
        return Err(Failure::CaptivePortal.error());
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut response = httparse::Response::new(&mut headers);
    response
        .parse(bytes)
        .map_err(|_| Failure::InvalidProtocol.error())?;
    if status != 101
        || response.version != Some(1)
        || !token(response.headers, "Upgrade", "websocket")
        || !token(response.headers, "Connection", "upgrade")
        || header(response.headers, "Sec-WebSocket-Accept") != Some(accept)
        || response.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("Sec-WebSocket-Extensions")
                || h.name.eq_ignore_ascii_case("Sec-WebSocket-Protocol")
        })
    {
        return Err(Failure::UpgradeRejected { status }.error());
    }
    Ok(())
}

pub(crate) enum State {
    Native,
    Detect,
    HttpServer(Headers),
    HttpClient { headers: Headers, accept: String },
    WebSocket(WebSocket),
    Closing,
}
impl State {
    pub fn client(host: &str, port: u16) -> io::Result<(Self, Vec<u8>)> {
        let key = STANDARD.encode(random::<16>()?);
        let request = format!("GET {WEBSOCKET_PATH} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").into_bytes();
        Ok((
            Self::HttpClient {
                headers: Headers::default(),
                accept: accept_key(&key),
            },
            request,
        ))
    }
    pub fn alpn(&mut self, alpn: Option<&[u8]>) -> io::Result<()> {
        match self {
            Self::Detect => {
                *self = if alpn == Some(crate::wire::ALPN) {
                    Self::Native
                } else if alpn.is_none() || alpn == Some(HTTP_ALPN) {
                    Self::HttpServer(Headers::default())
                } else {
                    return Err(Failure::AlpnRejected.error());
                };
            }
            Self::Native if alpn != Some(crate::wire::ALPN) => {
                return Err(Failure::AlpnRejected.error())
            }
            Self::HttpClient { .. } if alpn.is_some() && alpn != Some(HTTP_ALPN) => {
                return Err(Failure::UpgradeRejected { status: 0 }.error())
            }
            _ => (),
        }
        Ok(())
    }
    pub fn ready(&self) -> bool {
        matches!(self, Self::Native | Self::WebSocket(_))
    }
    pub fn negotiate(&mut self, reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::HttpServer(headers) => {
                if let Some(bytes) = headers.read(reader)? {
                    match server_upgrade(&bytes) {
                        Ok(Some(response)) => {
                            *self = Self::WebSocket(WebSocket::new(false));
                            return Ok(Some(response));
                        }
                        _ => {
                            *self = Self::Closing;
                            let response = if bytes.starts_with(b"HEAD ") {
                                &DECOY[..DECOY.len() - 3]
                            } else {
                                DECOY
                            };
                            return Ok(Some(response.to_vec()));
                        }
                    }
                }
            }
            Self::HttpClient { headers, accept } => {
                if let Some(bytes) = headers.read_response(reader)? {
                    client_upgrade(&bytes, accept)?;
                    *self = Self::WebSocket(WebSocket::new(true));
                }
            }
            _ => (),
        }
        Ok(None)
    }
    pub fn encode(&self, record: &[u8]) -> io::Result<Vec<u8>> {
        let mut bytes = (record.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(record);
        match self {
            Self::Native => Ok(bytes),
            Self::WebSocket(ws) => frame(2, &bytes, ws.client),
            _ => Err(invalid("relay carrier is not ready")),
        }
    }
    pub fn expired(&self, limit: Duration) -> bool {
        matches!(self, Self::WebSocket(ws) if ws.started.is_some_and(|t| t.elapsed() >= limit))
    }
}

pub(crate) fn frame(opcode: u8, payload: &[u8], masked: bool) -> io::Result<Vec<u8>> {
    let mut output = vec![0x80 | opcode];
    let mask_bit = if masked { 0x80 } else { 0 };
    if payload.len() < 126 {
        output.push(mask_bit | payload.len() as u8);
    } else {
        output.push(mask_bit | 126);
        output.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    if masked {
        let mask = random::<4>()?;
        output.extend_from_slice(&mask);
        output.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    } else {
        output.extend_from_slice(payload);
    }
    Ok(output)
}

pub(crate) struct WebSocket {
    client: bool,
    header: Vec<u8>,
    payload: Vec<u8>,
    read: usize,
    message: Vec<u8>,
    fragmented: bool,
    started: Option<Instant>,
}
pub(crate) struct WsRead {
    pub record: Option<Vec<u8>>,
    pub response: Option<Vec<u8>>,
    pub close: bool,
}
impl WebSocket {
    fn new(client: bool) -> Self {
        Self {
            client,
            header: Vec::new(),
            payload: Vec::new(),
            read: 0,
            message: Vec::new(),
            fragmented: false,
            started: None,
        }
    }
    pub fn read(&mut self, reader: &mut dyn Read) -> io::Result<WsRead> {
        let empty = || WsRead {
            record: None,
            response: None,
            close: false,
        };
        // One bounded frame per turn. Control floods share socket/idle and quota bounds.
        loop {
            let needed = if self.header.len() < 2 {
                2
            } else {
                2 + if self.header[1] & 127 == 126 { 2 } else { 0 }
                    + if self.header[1] & 128 != 0 { 4 } else { 0 }
            };
            if self.header.len() == needed {
                break;
            }
            let mut byte = [0];
            match reader.read(&mut byte) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {
                    self.started.get_or_insert_with(Instant::now);
                    self.header.push(byte[0]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(empty()),
                Err(e) => return Err(e),
            }
        }
        let opcode = self.header[0] & 15;
        let final_frame = self.header[0] & 128 != 0;
        let masked = self.header[1] & 128 != 0;
        let short_len = self.header[1] & 127;
        if self.header[0] & 0x70 != 0
            || masked == self.client
            || short_len == 127
            || !matches!(opcode, 0 | 2 | 8 | 9 | 10)
            || (opcode >= 8 && (!final_frame || short_len >= 126))
            || (opcode == 0 && !self.fragmented)
            || (opcode == 2 && self.fragmented)
        {
            return Err(invalid("invalid WebSocket frame"));
        }
        let len = if short_len == 126 {
            u16::from_be_bytes(self.header[2..4].try_into().unwrap()) as usize
        } else {
            short_len as usize
        };
        if (short_len == 126 && len < 126)
            || len > MESSAGE_MAX
            || (opcode < 8 && self.message.len() + len > MESSAGE_MAX)
        {
            return Err(invalid("WebSocket message exceeds bound"));
        }
        self.payload.resize(len, 0);
        if self.read < len {
            match reader.read(&mut self.payload[self.read..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.read += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(empty()),
                Err(e) => return Err(e),
            }
            if self.read < len {
                return Ok(empty());
            }
        }
        if masked {
            let mask = &self.header[self.header.len() - 4..];
            for (i, byte) in self.payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        self.header.clear();
        self.read = 0;
        let payload = std::mem::take(&mut self.payload);
        let mut result = empty();
        match opcode {
            8 => {
                if payload.len() == 1 {
                    return Err(invalid("invalid WebSocket close"));
                }
                if payload.len() >= 2 {
                    let code = u16::from_be_bytes(payload[..2].try_into().unwrap());
                    if !(1000..=1014).contains(&code) && !(3000..=4999).contains(&code)
                        || matches!(code, 1004..=1006)
                        || std::str::from_utf8(&payload[2..]).is_err()
                    {
                        return Err(invalid("invalid WebSocket close"));
                    }
                }
                result.response = Some(frame(8, &payload, self.client)?);
                result.close = true;
            }
            9 => result.response = Some(frame(10, &payload, self.client)?),
            10 => (),
            _ => {
                self.message.extend_from_slice(&payload);
                self.fragmented = !final_frame;
                if final_frame {
                    let bytes = std::mem::take(&mut self.message);
                    if bytes.len() < 5
                        || bytes.len() > MESSAGE_MAX
                        || u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize
                            != bytes.len() - 4
                    {
                        return Err(invalid("invalid WebSocket relay record"));
                    }
                    result.record = Some(bytes[4..].to_vec());
                }
            }
        }
        if !self.fragmented {
            self.started = None;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn websocket_upgrade_matches_rfc6455() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        let (_, request) = State::client("localhost", 443).unwrap();
        let response = server_upgrade(&request).unwrap().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 101"));
        assert!(server_upgrade(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap()
            .is_none());
        let mut server = State::HttpServer(Headers::default());
        let response = server
            .negotiate(&mut &b"HEAD / HTTP/1.1\r\nHost: localhost\r\n\r\n"[..])
            .unwrap()
            .unwrap();
        assert!(response.ends_with(b"\r\n\r\n"));
        assert!(response.windows(17).any(|w| w == b"Content-Length: 3"));
    }
    #[test]
    fn websocket_bounds_masking_fragmentation_and_controls() {
        for client in [false, true] {
            let mut ws = WebSocket::new(client);
            let mut record = 3u32.to_be_bytes().to_vec();
            record.extend([1, 2, 3]);
            let received = ws
                .read(&mut &frame(2, &record, !client).unwrap()[..])
                .unwrap();
            assert_eq!(received.record.unwrap(), [1, 2, 3]);
            let received = ws
                .read(&mut &frame(9, b"ping", !client).unwrap()[..])
                .unwrap();
            let pong = received.response.unwrap();
            assert_eq!(pong[0], 0x8a);
            assert_eq!(pong[1] & 128 != 0, client);
            assert!(WebSocket::new(!client)
                .read(&mut &pong[..])
                .unwrap()
                .record
                .is_none());
            assert!(ws
                .read(&mut &frame(2, &record, client).unwrap()[..])
                .is_err());
            let mut ws = WebSocket::new(client);
            assert!(ws
                .read(&mut &frame(2, &vec![0; MESSAGE_MAX + 1], !client).unwrap()[..])
                .is_err());
            let mut ws = WebSocket::new(client);
            let mut fragment = frame(2, &record[..2], !client).unwrap();
            fragment[0] &= 0x7f;
            assert!(ws.read(&mut &fragment[..]).unwrap().record.is_none());
            // Ping can be interleaved in a fragmented binary message.
            ws.read(&mut &frame(9, b"ping", !client).unwrap()[..])
                .unwrap();
            assert_eq!(
                ws.read(&mut &frame(0, &record[2..], !client).unwrap()[..])
                    .unwrap()
                    .record
                    .unwrap(),
                [1, 2, 3]
            );
            let close = ws
                .read(&mut &frame(8, &1000u16.to_be_bytes(), !client).unwrap()[..])
                .unwrap();
            assert!(close.close && close.response.is_some());
        }
        let mut headers = Headers::default();
        assert!(headers.read(&mut &[b'x'; HTTP_MAX][..]).is_err());
    }
}
