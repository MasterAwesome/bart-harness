use rustix::net::{
    self, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use std::{
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
};

#[derive(Clone, Copy)]
pub(crate) struct Streams<'a> {
    pub(crate) stdout: BorrowedFd<'a>,
    pub(crate) stderr: BorrowedFd<'a>,
}

const READ_BYTES: usize = 16 * 1024;
const READS_PER_WAKE: usize = 16;

pub(crate) fn send(socket: impl AsFd, streams: Streams<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    net::sockopt::set_socket_nosigpipe(&socket, true)?;

    #[cfg(target_os = "linux")]
    let flags = SendFlags::NOSIGNAL;
    #[cfg(target_os = "macos")]
    let flags = SendFlags::empty();

    let fds = [streams.stdout, streams.stderr];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    assert!(ancillary.push(SendAncillaryMessage::ScmRights(&fds)));
    let n = net::sendmsg(socket, &[IoSlice::new(b"D")], &mut ancillary, flags)?;
    if n != 1 {
        return Err(io::Error::other("incomplete job handoff"));
    }
    Ok(())
}

pub(super) fn receive(socket: &UnixStream) -> io::Result<[Option<OwnedFd>; 2]> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(super::CLEANUP))?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let mut marker = [0];
    let received = net::recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut marker)],
        &mut ancillary,
        RecvFlags::empty(),
    )?;
    let mut fds = Vec::new();
    for message in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(rights) = message {
            fds.extend(rights);
        }
    }
    if received.bytes != 1
        || marker != *b"D"
        || received.flags.contains(rustix::net::ReturnFlags::CTRUNC)
        || fds.len() != 2
    {
        return Err(io::Error::other("invalid job output handoff"));
    }
    let [stdout, stderr]: [OwnedFd; 2] = fds
        .try_into()
        .map_err(|_| io::Error::other("invalid handoff descriptors"))?;
    for input in [&stdout, &stderr] {
        let flags = rustix::fs::fcntl_getfl(input)?;
        rustix::fs::fcntl_setfl(input, flags | rustix::fs::OFlags::NONBLOCK)?;
    }
    Ok([Some(stdout), Some(stderr)])
}

pub(super) fn drain(stream: &mut Option<OwnedFd>) -> io::Result<()> {
    let Some(input) = stream else {
        return Ok(());
    };
    let mut buffer = [0; READ_BYTES];
    for _ in 0..READS_PER_WAKE {
        match rustix::io::read(&*input, &mut buffer) {
            Ok(0) => {
                *stream = None;
                break;
            }
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn received_streams_are_nonblocking_and_discard_output_until_eof() {
        let (control, receiver) = UnixStream::pair().unwrap();
        let (input, mut writer) = UnixStream::pair().unwrap();
        let (error_input, mut error_writer) = UnixStream::pair().unwrap();
        send(
            &control,
            Streams {
                stdout: input.as_fd(),
                stderr: error_input.as_fd(),
            },
        )
        .unwrap();
        let mut streams = receive(&receiver).unwrap();
        drop(input);
        drop(error_input);
        for stream in &mut streams {
            drain(stream).unwrap();
        }
        writer.write_all(b"abcdef").unwrap();
        error_writer.write_all(b"12345").unwrap();
        drop(writer);
        drop(error_writer);
        for stream in &mut streams {
            drain(stream).unwrap();
            assert!(stream.is_none());
        }
    }
}
