//! Messages between `anythingd` and its clients over a Unix socket.
//!
//! Every message is a little-endian `u32` length followed by that many bytes:
//! a tag byte and the fields. Strings are UTF-8 with a `u32` length. Sizes
//! are limited on both sides; anything malformed ends the connection.
//!
//! The service keeps the last search result per connection, so a client
//! asks for a search once and then fetches the rows it shows.

use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};

pub const VERSION: u32 = 1;
/// Default socket of the system service.
pub const SOCKET: &str = "/run/anything/anything.sock";

/// Largest request the service reads.
pub const MAX_REQUEST: usize = 64 * 1024;
/// Largest response a client reads.
pub const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Most rows per request.
pub const MAX_ROWS: u32 = 2000;
/// Longest string in a request.
const MAX_STRING: usize = 4096;
/// Most items in a list in a request.
const MAX_LIST: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    Path,
    Size,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello { version: u32 },
    Status,
    /// Searches what the asking user may see, limited to `include` minus
    /// `exclude` (the user's scan settings, used as a filter).
    Search { query: String, sort: Sort, descending: bool, include: Vec<String>, exclude: Vec<String> },
    /// Rows of the last search.
    Rows { start: u32, count: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    Loading,
    Scanning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// Folder the entry is in.
    pub dir: String,
    pub is_dir: bool,
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub modified: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Hello { version: u32 },
    /// `generation` changes whenever the index changes; a client that sees a
    /// new one searches again.
    Status { state: State, entries: u64, generation: u64 },
    Searched { total: u32, micros: u32, generation: u64 },
    Rows { start: u32, rows: Vec<Row> },
    Error { message: String },
}

// --- encoding ---------------------------------------------------------------

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    fn list(&mut self, items: &[String]) {
        self.u32(items.len() as u32);
        for item in items {
            self.str(item);
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    max_string: usize,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.data.len() < n {
            return Err(bad("message too short"));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        if len > self.max_string {
            return Err(bad("string too long"));
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| bad("string not UTF-8"))
    }
    fn list(&mut self) -> io::Result<Vec<String>> {
        let len = self.u32()? as usize;
        if len > MAX_LIST {
            return Err(bad("list too long"));
        }
        (0..len).map(|_| self.str()).collect()
    }
    fn end(&self) -> io::Result<()> {
        if self.data.is_empty() { Ok(()) } else { Err(bad("trailing bytes")) }
    }
}

fn sort_code(sort: Sort) -> u8 {
    match sort {
        Sort::Name => 0,
        Sort::Path => 1,
        Sort::Size => 2,
        Sort::Modified => 3,
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Request::Hello { version } => {
                w.u8(1);
                w.u32(*version);
            }
            Request::Status => w.u8(2),
            Request::Search { query, sort, descending, include, exclude } => {
                w.u8(3);
                w.str(query);
                w.u8(sort_code(*sort));
                w.u8(*descending as u8);
                w.list(include);
                w.list(exclude);
            }
            Request::Rows { start, count } => {
                w.u8(4);
                w.u32(*start);
                w.u32(*count);
            }
        }
        w.0
    }

    pub fn decode(data: &[u8]) -> io::Result<Request> {
        let mut r = Reader { data, max_string: MAX_STRING };
        let request = match r.u8()? {
            1 => Request::Hello { version: r.u32()? },
            2 => Request::Status,
            3 => Request::Search {
                query: r.str()?,
                sort: match r.u8()? {
                    0 => Sort::Name,
                    1 => Sort::Path,
                    2 => Sort::Size,
                    3 => Sort::Modified,
                    _ => return Err(bad("unknown sort")),
                },
                descending: r.u8()? != 0,
                include: r.list()?,
                exclude: r.list()?,
            },
            4 => {
                let (start, count) = (r.u32()?, r.u32()?);
                if count > MAX_ROWS {
                    return Err(bad("too many rows"));
                }
                Request::Rows { start, count }
            }
            _ => return Err(bad("unknown request")),
        };
        r.end()?;
        Ok(request)
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        match self {
            Response::Hello { version } => {
                w.u8(1);
                w.u32(*version);
            }
            Response::Status { state, entries, generation } => {
                w.u8(2);
                w.u8(match state {
                    State::Ready => 0,
                    State::Loading => 1,
                    State::Scanning => 2,
                });
                w.u64(*entries);
                w.u64(*generation);
            }
            Response::Searched { total, micros, generation } => {
                w.u8(3);
                w.u32(*total);
                w.u32(*micros);
                w.u64(*generation);
            }
            Response::Rows { start, rows } => {
                w.u8(4);
                w.u32(*start);
                w.u32(rows.len() as u32);
                for row in rows {
                    w.str(&row.name);
                    w.str(&row.dir);
                    w.u8(row.is_dir as u8);
                    w.u64(row.size);
                    w.u64(row.modified as u64);
                }
            }
            Response::Error { message } => {
                w.u8(5);
                w.str(message);
            }
        }
        w.0
    }

    pub fn decode(data: &[u8]) -> io::Result<Response> {
        let mut r = Reader { data, max_string: MAX_RESPONSE };
        let response = match r.u8()? {
            1 => Response::Hello { version: r.u32()? },
            2 => Response::Status {
                state: match r.u8()? {
                    0 => State::Ready,
                    1 => State::Loading,
                    _ => State::Scanning,
                },
                entries: r.u64()?,
                generation: r.u64()?,
            },
            3 => Response::Searched { total: r.u32()?, micros: r.u32()?, generation: r.u64()? },
            4 => {
                let start = r.u32()?;
                let count = r.u32()?;
                if count > MAX_ROWS {
                    return Err(bad("too many rows"));
                }
                let mut rows = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    rows.push(Row {
                        name: r.str()?,
                        dir: r.str()?,
                        is_dir: r.u8()? != 0,
                        size: r.u64()?,
                        modified: r.u64()? as i64,
                    });
                }
                Response::Rows { start, rows }
            }
            5 => Response::Error { message: r.str()? },
            _ => return Err(bad("unknown response")),
        };
        r.end()?;
        Ok(response)
    }
}

// --- framing ----------------------------------------------------------------

pub fn write_frame(out: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

/// Reads one frame of at most `max` bytes; `None` at a clean end of stream.
pub fn read_frame(input: &mut impl Read, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match input.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(bad("frame too large"));
    }
    let mut payload = vec![0u8; len];
    input.read_exact(&mut payload)?;
    Ok(Some(payload))
}

// --- client -----------------------------------------------------------------

/// A blocking connection to the service.
pub struct Client {
    stream: UnixStream,
}

impl Client {
    pub fn connect(socket: &Path) -> io::Result<Client> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut client = Client { stream };
        match client.ask(&Request::Hello { version: VERSION })? {
            Response::Hello { version } if version == VERSION => Ok(client),
            Response::Hello { version } => Err(bad(&format!("service speaks version {version}, not {VERSION}"))),
            other => Err(bad(&format!("unexpected answer {other:?}"))),
        }
    }

    pub fn ask(&mut self, request: &Request) -> io::Result<Response> {
        write_frame(&mut self.stream, &request.encode())?;
        let payload = read_frame(&mut self.stream, MAX_RESPONSE)?.ok_or_else(|| bad("service closed"))?;
        match Response::decode(&payload)? {
            Response::Error { message } => Err(io::Error::other(message)),
            response => Ok(response),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let requests = [
            Request::Hello { version: 1 },
            Request::Status,
            Request::Search {
                query: "Rechnung*.pdf".into(),
                sort: Sort::Modified,
                descending: true,
                include: vec!["/".into()],
                exclude: vec!["/home/x".into(), "node_modules".into()],
            },
            Request::Rows { start: 10, count: 50 },
        ];
        for request in requests {
            assert_eq!(Request::decode(&request.encode()).unwrap(), request);
        }
        let responses = [
            Response::Hello { version: 1 },
            Response::Status { state: State::Scanning, entries: 42, generation: 7 },
            Response::Searched { total: 3, micros: 900, generation: 7 },
            Response::Rows {
                start: 0,
                rows: vec![Row { name: "ü.txt".into(), dir: "/a".into(), is_dir: false, size: 5, modified: -1 }],
            },
            Response::Error { message: "nope".into() },
        ];
        for response in responses {
            assert_eq!(Response::decode(&response.encode()).unwrap(), response);
        }
    }

    #[test]
    fn rejects_malformed_requests() {
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[9]).is_err());
        // Too many rows.
        assert!(Request::decode(&Request::Rows { start: 0, count: MAX_ROWS + 1 }.encode()).is_err());
        // A string claiming to be huge.
        let mut huge = vec![3];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(Request::decode(&huge).is_err());
        // Trailing garbage.
        let mut trailing = Request::Status.encode();
        trailing.push(0);
        assert!(Request::decode(&trailing).is_err());
        // Frames above the limit are refused before reading them.
        let mut frame = Vec::new();
        frame.extend_from_slice(&((MAX_REQUEST + 1) as u32).to_le_bytes());
        assert!(read_frame(&mut frame.as_slice(), MAX_REQUEST).is_err());
    }
}
