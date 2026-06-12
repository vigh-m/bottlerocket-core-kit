/*
 *  Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *  Copyright (C) 2016-2023 Red Hat, Inc.
 *  Copyright (C) 2023 David Rheinsberg <david@readahead.eu>
 *  Copyright (C) 2023 Tom Gundersen <teg@jklm.no>
 *
 *  SPDX-License-Identifier: Apache-2.0
 *  Originally derived from:
 *  https://github.com/bus1/dbus-broker/blob/b0db0890d1254477cf832e5f9f0a798360c80fd9/src/launch/main.c#L104
 *
 *  Changes for Bottlerocket:
 *    - Combine the referenced function with a simple implementation of sd_listen_fds
 *      https://github.com/systemd/systemd-stable/blob/356c54394add8c6a1d52773852c23656590dc33b/src/libsystemd/sd-daemon/sd-daemon.c#L42
 */

//! Socket management and systemd integration.
//!
//! This module provides utilities for managing Unix sockets used for communication
//! with dbus-broker and systemd, including socket activation and notification.

use crate::error::{self, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use snafu::{ensure, ResultExt};
use std::env;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use tokio::net::UnixStream;

/// The start of the file descriptors provided by systemd's socket activation
const SD_LISTEN_FDS_START: u8 = 3;
/// The path to the journal socket
const JOURNAL_SOCKET_PATH: &str = "/run/systemd/journal/socket";

const LISTEN_PID_ENVVAR: &str = "LISTEN_PID";
const LISTEN_FDS_ENVVAR: &str = "LISTEN_FDS";
const NOTIFY_SOCKET_ENVVAR: &str = "NOTIFY_SOCKET";

/// Retrieves the systemd-provided listener socket file descriptor.
///
/// Validates that exactly one socket was provided via systemd socket activation
/// and configures it with the required flags for dbus-broker operation.
pub fn listener_socket() -> Result<u8> {
    let listen_pid = std::env::var(LISTEN_PID_ENVVAR).context(error::MissingEnvVarSnafu {
        what: LISTEN_PID_ENVVAR.to_string(),
    })?;

    let current_pid = std::process::id().to_string();
    ensure!(
        listen_pid == current_pid,
        error::UnexpectedPidSnafu {
            expected: current_pid,
            found: listen_pid
        }
    );

    let listen_fds = std::env::var(LISTEN_FDS_ENVVAR).context(error::MissingEnvVarSnafu {
        what: LISTEN_FDS_ENVVAR.to_string(),
    })?;

    let fd_count: u32 = listen_fds
        .parse()
        .context(error::InvalidFileDescriptorCountSnafu { count: listen_fds })?;

    ensure!(
        fd_count == 1,
        error::UnexpectedFileDescriptorCountSnafu { found: fd_count }
    );

    // The file descriptor isn't owned by whippet and it is managed by systemd.
    // SAFETY: SD_LISTEN_FDS_START (fd 3) is guaranteed valid by systemd socket
    // activation after validating LISTEN_PID and LISTEN_FDS above.
    let fd = unsafe { BorrowedFd::borrow_raw(SD_LISTEN_FDS_START as i32) };

    // The listener socket has to be configured as O_NONBLOCK, otherwise clients that connect to
    // the dbus socket will hang
    set_nonblock(fd)?;

    // Return the start of the file descriptors, as only one is expected
    // See:
    // https://github.com/bus1/dbus-broker/blob/b0db0890d1254477cf832e5f9f0a798360c80fd9/src/launch/main.c#L104
    Ok(SD_LISTEN_FDS_START)
}

/// Creates a Unix socket pair for dbus-broker controller communication.
///
/// Returns a pair of connected Unix stream sockets with CLOEXEC flags cleared
/// to allow inheritance by the child process.
#[must_use = "socket pair must be used for communication"]
pub fn controller_pair() -> Result<(UnixStream, UnixStream)> {
    let (stream1, stream2) = UnixStream::pair().context(error::SocketPairSnafu)?;

    clear_cloexec(stream1.as_fd())?;
    clear_cloexec(stream2.as_fd())?;

    Ok((stream1, stream2))
}

/// Creates a Unix datagram socket connected to the systemd journal.
///
/// Establishes connection to the journal socket for dbus-broker logging
/// and clears CLOEXEC to allow inheritance by the child process.
#[must_use = "journal socket must be used for logging"]
pub fn journal_socket() -> Result<UnixDatagram> {
    let socket = UnixDatagram::unbound().context(error::JournalSocketSnafu)?;

    socket
        .connect(JOURNAL_SOCKET_PATH)
        .context(error::JournalSocketSnafu)?;

    // Clear CLOEXEC flag so dbus-broker can inherit the socket
    clear_cloexec(socket.as_fd())?;

    Ok(socket)
}

/// Creates a Unix datagram socket for systemd service notifications.
///
/// Connects to the socket specified by the NOTIFY_SOCKET environment variable
/// for sending service readiness notifications to systemd.
#[must_use = "notification socket must be used to notify systemd"]
pub fn notify_socket() -> Result<UnixDatagram> {
    let notify_socket_path = env::var(NOTIFY_SOCKET_ENVVAR).context(error::MissingEnvVarSnafu {
        what: NOTIFY_SOCKET_ENVVAR.to_string(),
    })?;

    let socket = UnixDatagram::unbound().context(error::SystemdNotifySocketSnafu)?;

    socket
        .connect(Path::new(&notify_socket_path))
        .context(error::SystemdNotifySocketSnafu)?;

    Ok(socket)
}

/// Clears the CLOEXEC flag on a file descriptor.
///
/// Allows the file descriptor to be inherited by child processes,
/// which is required for dbus-broker socket inheritance.
pub fn clear_cloexec(fd: impl AsFd) -> Result<()> {
    let flags = fcntl(&fd, FcntlArg::F_GETFD).context(error::GetFdFlagsSnafu {
        fd: fd.as_fd().as_raw_fd(),
    })?;

    let mut fd_flags = FdFlag::from_bits_truncate(flags);
    fd_flags.remove(FdFlag::FD_CLOEXEC);

    nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFD(fd_flags)).context(
        error::SetFlagSnafu {
            what: "CLOEXEC",
            fd: fd.as_fd().as_raw_fd(),
        },
    )?;

    Ok(())
}

/// Sets the NONBLOCK flag for the file descriptor
pub fn set_nonblock(fd: impl AsFd) -> Result<()> {
    let current_flags = fcntl(&fd, FcntlArg::F_GETFL).context(error::GetFdFlagsSnafu {
        fd: fd.as_fd().as_raw_fd(),
    })?;

    let new_flags = OFlag::from_bits_truncate(current_flags) | OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(&fd, nix::fcntl::FcntlArg::F_SETFL(new_flags)).context(
        error::SetFlagSnafu {
            fd: fd.as_fd().as_raw_fd(),
            what: "O_NONBLOCK",
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    use std::os::unix::net::UnixStream as StdUnixStream;

    #[test]
    fn clear_cloexec_removes_flag() {
        // UnixStream::pair() creates fds with CLOEXEC set by default
        let (sock, _peer) = StdUnixStream::pair().unwrap();
        clear_cloexec(&sock).unwrap();

        let flags = fcntl(sock.as_fd(), FcntlArg::F_GETFD).unwrap();
        let fd_flags = FdFlag::from_bits_truncate(flags);
        assert!(!fd_flags.contains(FdFlag::FD_CLOEXEC));
    }

    #[test]
    fn clear_cloexec_idempotent() {
        let (sock, _peer) = StdUnixStream::pair().unwrap();
        clear_cloexec(&sock).unwrap();
        // Calling again should not error
        clear_cloexec(&sock).unwrap();
    }

    #[test]
    fn set_nonblock_sets_flag() {
        let (sock, _peer) = StdUnixStream::pair().unwrap();
        set_nonblock(&sock).unwrap();

        let flags = fcntl(sock.as_fd(), FcntlArg::F_GETFL).unwrap();
        let oflags = OFlag::from_bits_truncate(flags);
        assert!(oflags.contains(OFlag::O_NONBLOCK));
    }

    #[test]
    fn set_nonblock_idempotent() {
        let (sock, _peer) = StdUnixStream::pair().unwrap();
        set_nonblock(&sock).unwrap();
        set_nonblock(&sock).unwrap();
    }

    #[test]
    fn works_with_different_fd_types() {
        use std::os::unix::net::UnixDatagram;

        // UnixDatagram - same type used by journal_socket()
        let dg = UnixDatagram::unbound().unwrap();
        clear_cloexec(&dg).unwrap();
        set_nonblock(&dg).unwrap();

        // Verify both flags applied
        let fd_flags = FdFlag::from_bits_truncate(fcntl(dg.as_fd(), FcntlArg::F_GETFD).unwrap());
        let oflags = OFlag::from_bits_truncate(fcntl(dg.as_fd(), FcntlArg::F_GETFL).unwrap());
        assert!(!fd_flags.contains(FdFlag::FD_CLOEXEC));
        assert!(oflags.contains(OFlag::O_NONBLOCK));
    }
}
