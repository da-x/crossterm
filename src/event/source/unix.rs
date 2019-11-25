use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use mio::{unix::EventedFd, Events, Poll, PollOpt, Ready, Token};
use signal_hook::iterator::Signals;

use crate::Result;

use super::super::{
    source::EventSource,
    sys::unix::{parse_event, tty_fd, FileDesc},
    timeout::PollTimeout,
    Event, InternalEvent,
};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
const WAKE_TOKEN: Token = Token(2);

const TTY_BUFFER_SIZE: usize = 8_192;
const TTY_BUFFER_THRESHOLD: usize = 512;

/// Creates a new pipe and returns `(read, write)` file descriptors.
fn pipe() -> Result<(FileDesc, FileDesc)> {
    let (read_fd, write_fd) = unsafe {
        let mut pipe_fds: [libc::c_int; 2] = [0; 2];
        if libc::pipe(pipe_fds.as_mut_ptr()) == -1 {
            return Err(io::Error::last_os_error().into());
        }
        (pipe_fds[0], pipe_fds[1])
    };

    let read_fd = FileDesc::new(read_fd, true);
    let write_fd = FileDesc::new(write_fd, true);

    Ok((read_fd, write_fd))
}

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    tty_buffer: Vec<u8>,
    tty_buffer_head_index: usize,
    tty_buffer_byte_count: usize,
    tty_fd: FileDesc,
    signals: Signals,
    wake_read_fd: FileDesc,
    wake_write_fd: FileDesc,
    internal_events: VecDeque<InternalEvent>,
}

impl UnixInternalEventSource {
    pub fn new() -> Result<Self> {
        Ok(UnixInternalEventSource::from_file_descriptor(tty_fd()?)?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc) -> Result<Self> {
        let poll = Poll::new()?;

        // PollOpt::level vs PollOpt::edge mio documentation:
        //
        // > With edge-triggered events, operations must be performed on the Evented type until
        // > WouldBlock is returned.
        //
        // TL;DR - DO NOT use PollOpt::edge.
        //
        // Because of the `try_read` nature (loop with returns) we can't use `PollOpt::edge`. All
        // `Evented` handles MUST be registered with the `PollOpt::level`.
        //
        // If you have to use `PollOpt::edge` and there's no way how to do it with the `PollOpt::level`,
        // be aware that the whole `TtyInternalEventSource` have to be rewritten
        // (read everything from each `Evented`, process without returns, store all InternalEvent events
        // into a buffer and then return first InternalEvent, etc.). Even these changes wont be
        // enough, because `Poll::poll` wont fire again until additional `Evented` event happens and
        // we can still have a buffer filled with InternalEvent events.
        let tty_raw_fd = input_fd.raw_fd();
        let tty_ev = EventedFd(&tty_raw_fd);
        poll.register(&tty_ev, TTY_TOKEN, Ready::readable(), PollOpt::level())?;

        let signals = Signals::new(&[signal_hook::SIGWINCH])?;
        poll.register(&signals, SIGNAL_TOKEN, Ready::readable(), PollOpt::level())?;

        let (wake_read_fd, wake_write_fd) = pipe()?;
        let wake_read_raw_fd = wake_read_fd.raw_fd();
        let wake_read_ev = EventedFd(&wake_read_raw_fd);
        poll.register(
            &wake_read_ev,
            WAKE_TOKEN,
            Ready::readable(),
            PollOpt::level(),
        )?;

        let mut tty_buffer = Vec::with_capacity(TTY_BUFFER_SIZE);
        unsafe {
            tty_buffer.set_len(TTY_BUFFER_SIZE);
        }

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            tty_buffer,
            tty_buffer_head_index: 0,
            tty_buffer_byte_count: 0,
            tty_fd: input_fd,
            signals,
            wake_read_fd,
            wake_write_fd,
            internal_events: VecDeque::with_capacity(8),
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> Result<Option<InternalEvent>> {
        // Do we have an event from the past? Return immediately.
        if let Some(event) = self.internal_events.pop_front() {
            return Ok(Some(event));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            let event_count = self.poll.poll(&mut self.events, timeout.leftover())?;

            match event_count {
                event_count if event_count > 0 => {
                    let events_count = self
                        .events
                        .iter()
                        .map(|x| x.token())
                        .collect::<Vec<Token>>();

                    for event in events_count {
                        match event {
                            TTY_TOKEN => {
                                if self.tty_buffer_head_index + self.tty_buffer_byte_count
                                    >= TTY_BUFFER_SIZE
                                {
                                    panic!("There's something bad with event processing");
                                }

                                // How many bytes we can read/fit into our buffer?
                                let max_read_count = TTY_BUFFER_SIZE
                                    - self.tty_buffer_head_index
                                    - self.tty_buffer_byte_count;

                                // Read as many as possible bytes
                                let read_count = self.tty_fd.read(
                                    &mut self.tty_buffer
                                        [self.tty_buffer_head_index + self.tty_buffer_byte_count..],
                                    max_read_count,
                                )?;

                                // If we read something ...
                                if read_count > 0 {
                                    // ... check if there's more (buffer too small, ...).
                                    let input_available = self
                                        .poll
                                        .poll(&mut self.events, Some(Duration::from_secs(0)))
                                        .map(|x| x > 0)?;

                                    // How many bytes we should process (what we had + new ones)
                                    let mut byte_count_to_process =
                                        self.tty_buffer_byte_count + read_count;

                                    let mut consumed_bytes = 0;

                                    // Loop until all bytes are processed
                                    while byte_count_to_process > 0 {
                                        // We have to use this loop, because `parse_event`, `parse_csi`, ...
                                        // functions are not efficient. They're matching first bytes and also
                                        // last byte (csi xterm mouse where last 'm'/'M' says up/down), etc.
                                        //
                                        // In other words, we try to parse with 1 byte, 2 bytes, 3 bytes,
                                        // 4 bytes, 5 bytes, ... until the parser error or returns an event.
                                        //
                                        // If we will switch to the anes parser (two phases parsing), we can
                                        // easily avoid this inner for loop. The reason is that the anes parser
                                        // knows how to parse csi sequence without a meaning (knows when the csi
                                        // sequence ends) and then it gives it a meaning. We do not need to
                                        // advance with byte by byte here.
                                        for i in 1..=byte_count_to_process {
                                            // More bytes to read? Yes if we're not at the end of the buffer
                                            // or poll says that there's more and we're at the end of the buffer
                                            let more = i < byte_count_to_process || input_available;

                                            match parse_event(
                                                &self.tty_buffer[self.tty_buffer_head_index
                                                    ..self.tty_buffer_head_index + i],
                                                more,
                                            ) {
                                                Ok(None) => {
                                                    if i == byte_count_to_process {
                                                        // We're at the end of buffer, just break the
                                                        // outer while loop
                                                        byte_count_to_process = 0;
                                                    }
                                                }
                                                Ok(Some(ie)) => {
                                                    // We've got event, push it to the queue
                                                    self.internal_events.push_back(ie);

                                                    // Increase number of consumed bytes
                                                    consumed_bytes += i;
                                                    // Move the head
                                                    self.tty_buffer_head_index += i;
                                                    // Decrease number of bytes to process
                                                    byte_count_to_process -= i;
                                                    // Break the inner for loop
                                                    break;
                                                }
                                                Err(_) => {
                                                    // Increase number of consumed bytes
                                                    consumed_bytes += i;
                                                    // Move the head
                                                    self.tty_buffer_head_index += i;
                                                    // Decrease number of bytes to process
                                                    byte_count_to_process -= i;
                                                    // Break the inner for loop
                                                    break;
                                                }
                                            };
                                        }
                                    }

                                    // Update number of bytes left for future processing
                                    self.tty_buffer_byte_count += read_count - consumed_bytes;

                                    // If we're near the end of buffer ...
                                    if self.tty_buffer_head_index + TTY_BUFFER_THRESHOLD
                                        >= TTY_BUFFER_SIZE - 1
                                    {
                                        // ... and there're some bytes for future processing ...
                                        if self.tty_buffer_byte_count > 0 {
                                            // ... copy them to the buffer beginning ...
                                            self.tty_buffer.copy_within(
                                                self.tty_buffer_head_index
                                                    ..self.tty_buffer_head_index
                                                        + self.tty_buffer_byte_count,
                                                0,
                                            );
                                        }
                                        // ... and move the head index back to the beginning.
                                        self.tty_buffer_head_index = 0;
                                    }

                                    // Return an event if we've got one
                                    if let Some(event) = self.internal_events.pop_front() {
                                        return Ok(Some(event));
                                    }
                                }
                            }
                            SIGNAL_TOKEN => {
                                for signal in &self.signals {
                                    match signal as libc::c_int {
                                        signal_hook::SIGWINCH => {
                                            // TODO Should we remove tput?
                                            //
                                            // This can take a really long time, because terminal::size can
                                            // launch new process (tput) and then it parses its output. It's
                                            // not a really long time from the absolute time point of view, but
                                            // it's a really long time from the mio, async-std/tokio executor, ...
                                            // point of view.
                                            let new_size = crate::terminal::size()?;
                                            return Ok(Some(InternalEvent::Event(Event::Resize(
                                                new_size.0, new_size.1,
                                            ))));
                                        }
                                        _ => unreachable!(),
                                    };
                                }
                            }
                            WAKE_TOKEN => {
                                // Something happened on the self pipe. Try to read single byte
                                // (see wake() fn) and ignore result. If we can't read the byte,
                                // mio Poll::poll will fire another event with WAKE_TOKEN.
                                let _ = self.wake_read_fd.read_byte();
                                return Ok(None);
                            }
                            _ => {}
                        }
                    }
                }
                _ => return Ok(None),
            };

            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    fn wake(&self) {
        // DO NOT write more than 1 byte. See try_read & WAKE_TOKEN
        // handling - it reads just 1 byte. If you write more than
        // 1 byte, lets say N, then the try_read will be woken up
        // N times.
        let _ = self.wake_write_fd.write(&[0x57]);
    }
}
