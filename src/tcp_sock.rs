use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use maidsafe_utilities::serialisation::{deserialise_from, serialise_into};
use mio::tcp::TcpStream;
use mio::{Evented, Poll, PollOpt, Ready, Token};
use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Debug, Formatter};
use std::io::{self, Cursor, ErrorKind, Read, Write};
use std::mem;
use std::net::{Shutdown, SocketAddr};
use std::time::Duration;
use std::time::Instant;
use {Priority, SocketError, MAX_MSG_AGE_SECS, MAX_PAYLOAD_SIZE, MSG_DROP_PRIORITY};

pub struct TcpSock {
    inner: Option<Inner>,
}

impl TcpSock {
    pub fn connect(addr: &SocketAddr) -> ::Res<Self> {
        let stream = TcpStream::connect(addr)?;
        Ok(Self::wrap(stream))
    }

    pub fn wrap(stream: TcpStream) -> Self {
        TcpSock {
            inner: Some(Inner {
                stream,
                read_buffer: Vec::new(),
                read_len: 0,
                write_queue: BTreeMap::new(),
                current_write: None,
            }),
        }
    }

    pub fn set_linger(&self, dur: Option<Duration>) -> ::Res<()> {
        let inner = self
            .inner
            .as_ref()
            .ok_or(SocketError::UninitialisedSocket)?;
        Ok(inner.stream.set_linger(dur)?)
    }

    pub fn local_addr(&self) -> ::Res<SocketAddr> {
        let inner = self
            .inner
            .as_ref()
            .ok_or(SocketError::UninitialisedSocket)?;
        Ok(inner.stream.local_addr()?)
    }

    pub fn peer_addr(&self) -> ::Res<SocketAddr> {
        let inner = self
            .inner
            .as_ref()
            .ok_or(SocketError::UninitialisedSocket)?;
        Ok(inner.stream.peer_addr()?)
    }

    pub fn take_error(&self) -> ::Res<Option<io::Error>> {
        let inner = self
            .inner
            .as_ref()
            .ok_or(SocketError::UninitialisedSocket)?;
        Ok(inner.stream.take_error()?)
    }

    // Read message from the socket. Call this from inside the `ready` handler.
    //
    // Returns:
    //   - Ok(Some(data)): data has been successfully read from the socket
    //   - Ok(None):       there is not enough data in the socket. Call `read`
    //                     again in the next invocation of the `ready` handler.
    //   - Err(error):     there was an error reading from the socket.
    pub fn read<T: DeserializeOwned>(&mut self) -> ::Res<Option<T>> {
        let inner = self
            .inner
            .as_mut()
            .ok_or(SocketError::UninitialisedSocket)?;
        inner.read()
    }

    // Write a message to the socket.
    //
    // Returns:
    //   - Ok(true):   the message has been successfully written.
    //   - Ok(false):  the message has been queued, but not yet fully written.
    //                 will be attempted in the next write schedule.
    //   - Err(error): there was an error while writing to the socket.
    pub fn write<T: Serialize>(&mut self, msg: Option<(T, Priority)>) -> ::Res<bool> {
        let inner = self
            .inner
            .as_mut()
            .ok_or(SocketError::UninitialisedSocket)?;
        inner.write(msg)
    }
}

impl Debug for TcpSock {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "TcpSock: initialised = {}", self.inner.is_some())
    }
}

impl Default for TcpSock {
    fn default() -> Self {
        TcpSock { inner: None }
    }
}

impl Evented for TcpSock {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            io::Error::new(
                ErrorKind::Other,
                format!("{}", SocketError::UninitialisedSocket),
            )
        })?;
        inner.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            io::Error::new(
                ErrorKind::Other,
                format!("{}", SocketError::UninitialisedSocket),
            )
        })?;
        inner.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        let inner = self.inner.as_ref().ok_or_else(|| {
            io::Error::new(
                ErrorKind::Other,
                format!("{}", SocketError::UninitialisedSocket),
            )
        })?;
        inner.deregister(poll)
    }
}

struct Inner {
    stream: TcpStream,
    read_buffer: Vec<u8>,
    read_len: usize,
    write_queue: BTreeMap<Priority, VecDeque<(Instant, Vec<u8>)>>,
    current_write: Option<Vec<u8>>,
}

impl Inner {
    // Read message from the socket. Call this from inside the `ready` handler.
    //
    // Returns:
    //   - Ok(Some(data)): data has been successfully read from the socket.
    //   - Ok(None):       there is not enough data in the socket. Call `read`
    //                     again in the next invocation of the `ready` handler.
    //   - Err(error):     there was an error reading from the socket.
    fn read<T: DeserializeOwned>(&mut self) -> ::Res<Option<T>> {
        if let Some(message) = self.read_from_buffer()? {
            return Ok(Some(message));
        }

        // the mio reading window is max at 64k (64 * 1024)
        let mut buffer = [0; 64 * 1024];
        let mut is_something_read = false;

        loop {
            match self.stream.read(&mut buffer) {
                Ok(bytes_rxd) => {
                    if bytes_rxd == 0 {
                        let e = Err(SocketError::ZeroByteRead);
                        if is_something_read {
                            return match self.read_from_buffer() {
                                r @ Ok(Some(_)) | r @ Err(_) => r,
                                Ok(None) => e,
                            };
                        } else {
                            return e;
                        }
                    }
                    self.read_buffer.extend_from_slice(&buffer[0..bytes_rxd]);
                    is_something_read = true;
                }
                Err(error) => {
                    return if error.kind() == ErrorKind::WouldBlock
                        || error.kind() == ErrorKind::Interrupted
                    {
                        if is_something_read {
                            self.read_from_buffer()
                        } else {
                            Ok(None)
                        }
                    } else {
                        Err(From::from(error))
                    }
                }
            }
        }
    }

    fn read_from_buffer<T: DeserializeOwned>(&mut self) -> ::Res<Option<T>> {
        let u32_size = mem::size_of::<u32>();

        if self.read_len == 0 {
            if self.read_buffer.len() < u32_size {
                return Ok(None);
            }

            self.read_len = Cursor::new(&self.read_buffer).read_u32::<LittleEndian>()? as usize;

            if self.read_len > MAX_PAYLOAD_SIZE {
                return Err(SocketError::PayloadSizeProhibitive);
            }

            self.read_buffer = self.read_buffer[u32_size..].to_owned();
        }

        if self.read_len > self.read_buffer.len() {
            return Ok(None);
        }

        let result = deserialise_from(&mut Cursor::new(&self.read_buffer))?;

        self.read_buffer = self.read_buffer[self.read_len..].to_owned();
        self.read_len = 0;

        Ok(Some(result))
    }

    // Write a message to the socket.
    //
    // Returns:
    //   - Ok(true):   the message has been successfully written.
    //   - Ok(false):  the message has been queued, but not yet fully written.
    //                 will be attempted in the next write schedule.
    //   - Err(error): there was an error while writing to the socket.
    fn write<T: Serialize>(&mut self, msg: Option<(T, Priority)>) -> ::Res<bool> {
        let expired_keys: Vec<u8> = self
            .write_queue
            .iter()
            .skip_while(|&(&priority, queue)| {
                priority < MSG_DROP_PRIORITY || // Don't drop high-priority messages.
                queue.front().map_or(true, |&(ref timestamp, _)| {
                    timestamp.elapsed().as_secs() <= MAX_MSG_AGE_SECS
                })
            }).map(|(&priority, _)| priority)
            .collect();
        let dropped_msgs: usize = expired_keys
            .iter()
            .filter_map(|priority| self.write_queue.remove(priority))
            .map(|queue| queue.len())
            .sum();
        if dropped_msgs > 0 {
            trace!(
                "Insufficient bandwidth. Dropping {} messages with priority >= {}.",
                dropped_msgs,
                expired_keys[0]
            );
        }

        if let Some((msg, priority)) = msg {
            let mut data = Cursor::new(Vec::with_capacity(mem::size_of::<u32>()));

            let _ = data.write_u32::<LittleEndian>(0);

            serialise_into(&msg, &mut data)?;

            let len = data.position() - mem::size_of::<u32>() as u64;
            data.set_position(0);
            data.write_u32::<LittleEndian>(len as u32)?;

            let entry = self
                .write_queue
                .entry(priority)
                .or_insert_with(|| VecDeque::with_capacity(10));
            entry.push_back((Instant::now(), data.into_inner()));
        }

        loop {
            let data = if let Some(data) = self.current_write.take() {
                data
            } else {
                let (key, (_time_stamp, data), empty) = match self.write_queue.iter_mut().next() {
                    Some((key, queue)) => (*key, unwrap!(queue.pop_front()), queue.is_empty()),
                    None => return Ok(true),
                };
                if empty {
                    let _ = self.write_queue.remove(&key);
                }
                data
            };

            match self.stream.write(&data) {
                Ok(bytes_txd) => {
                    if bytes_txd < data.len() {
                        self.current_write = Some(data[bytes_txd..].to_owned());
                    }
                }
                Err(error) => {
                    if error.kind() == ErrorKind::WouldBlock
                        || error.kind() == ErrorKind::Interrupted
                    {
                        self.current_write = Some(data);
                        return Ok(false);
                    } else {
                        return Err(From::from(error));
                    }
                }
            }
        }
    }
}

impl Evented for Inner {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.stream.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.stream.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.stream.deregister(poll)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}
