//! One optional explicit cancellation channel; no process-identity inference.
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct CancellationPipe {
    pipe: OwnedFd,
    cancelled: AtomicBool,
}
impl CancellationPipe {
    /// Transfer exclusive polling use of a private preexisting NONBLOCK read end.
    /// The producer retains only its write end after launching the controller.
    pub(super) fn from_inherited_fd(raw: RawFd) -> io::Result<Self> {
        Ok(Self {
            pipe: lean_toolchain::inherited_pipe::duplicate_inherited_read_pipe(raw)?,
            cancelled: AtomicBool::new(false),
        })
    }
    /// One nonblocking one-byte read per poll. Byte, EOF or a control failure
    /// cancels permanently; absence/interruption waits for the existing poll.
    pub(super) fn cancelled(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) {
            return true;
        }
        let mut byte = [0_u8; 1];
        let cancel = match rustix::io::read(&self.pipe, &mut byte) {
            Ok(_) => true,
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => false,
            Err(_) => true,
        };
        if cancel {
            self.cancelled.store(true, Ordering::Release);
        }
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "isolated real pipe fixtures assert descriptor properties"
)]
mod tests {
    use super::*;
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    use rustix::pipe::pipe;
    use std::os::fd::AsRawFd as _;
    fn nonblocking_pipe() -> (OwnedFd, OwnedFd) {
        let (read, write) = pipe().unwrap();
        fcntl_setfl(&read, fcntl_getfl(&read).unwrap() | OFlags::NONBLOCK).unwrap();
        fcntl_setfd(&read, FdFlags::CLOEXEC).unwrap();
        fcntl_setfd(&write, FdFlags::CLOEXEC).unwrap();
        (read, write)
    }
    #[test]
    fn empty_private_pipe_poll_is_bounded_and_preserves_description_flags() {
        let (read, _write) = nonblocking_pipe();
        let flags = fcntl_getfl(&read).unwrap();
        let cancel = CancellationPipe::from_inherited_fd(read.as_raw_fd()).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert!(!cancel.cancelled());
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert_eq!(fcntl_getfl(&read).unwrap(), flags);
    }
    #[test]
    fn byte_and_eof_cancel_while_the_caller_process_remains_alive() {
        let (read, write) = nonblocking_pipe();
        let cancel = CancellationPipe::from_inherited_fd(read.as_raw_fd()).unwrap();
        assert!(!cancel.cancelled());
        rustix::io::write(&write, b"x").unwrap();
        assert!(cancel.cancelled());
        assert!(cancel.cancelled());
        let (read, write) = nonblocking_pipe();
        let cancel = CancellationPipe::from_inherited_fd(read.as_raw_fd()).unwrap();
        drop(write);
        assert!(cancel.cancelled());
    }
    #[test]
    fn inherited_source_and_duplicate_are_cloexec_without_closing_source() {
        let (read, _write) = nonblocking_pipe();
        fcntl_setfd(&read, FdFlags::empty()).unwrap();
        let cancel = CancellationPipe::from_inherited_fd(read.as_raw_fd()).unwrap();
        assert!(fcntl_getfd(&read).unwrap().contains(FdFlags::CLOEXEC));
        assert!(fcntl_getfd(&cancel.pipe).unwrap().contains(FdFlags::CLOEXEC));
        assert!(fcntl_getfl(&read).unwrap().contains(OFlags::NONBLOCK));
    }
    #[test]
    fn invalid_regular_socket_write_and_blocking_sources_refuse() {
        for raw in [-1, 0, 1, 2, i32::MAX] {
            assert!(CancellationPipe::from_inherited_fd(raw).is_err());
        }
        let file = tempfile::tempfile().unwrap();
        assert!(CancellationPipe::from_inherited_fd(file.as_raw_fd()).is_err());
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(CancellationPipe::from_inherited_fd(socket.as_raw_fd()).is_err());
        let (_read, write) = nonblocking_pipe();
        assert!(CancellationPipe::from_inherited_fd(write.as_raw_fd()).is_err());
        let (read, _write) = pipe().unwrap();
        let flags = fcntl_getfl(&read).unwrap();
        assert!(CancellationPipe::from_inherited_fd(read.as_raw_fd()).is_err());
        assert_eq!(fcntl_getfl(&read).unwrap(), flags);
    }
}
