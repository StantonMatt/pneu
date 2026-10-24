//! Bounded descriptor-bearing frames on a private, inherited Unix socketpair.
//! No pathname or reconnect endpoint can be used to impersonate the UI child.
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
};

pub const LIMIT: usize = 1024 * 1024;

pub fn same_uid(socket: &UnixStream) -> Result<()> {
    #[cfg(target_os = "macos")]
    unsafe {
        let (mut uid, mut gid) = (0, 0);
        ensure!(
            libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) == 0,
            "UI peer credentials unavailable"
        );
        ensure!(uid == libc::geteuid(), "UI peer uid mismatch");
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        ensure!(
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len
            ) == 0,
            "UI peer credentials unavailable"
        );
        ensure!(cred.uid == libc::geteuid(), "UI peer uid mismatch");
    }
    Ok(())
}

pub fn send<T: Serialize>(
    socket: &mut UnixStream,
    value: &T,
    file: Option<&std::fs::File>,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= LIMIT,
        "UI frame exceeds limit"
    );
    let mut header = (bytes.len() as u32).to_be_bytes();
    // Ancillary data accompanies the first byte, never the JSON body. Exactly
    // one receiver reads this stream and every writer holds the same lock.
    let mut control = [0usize; 8];
    let mut iov = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: header.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    if let Some(file) = file {
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&message);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
            std::ptr::write_unaligned(
                libc::CMSG_DATA(cmsg).cast::<libc::c_int>(),
                file.as_raw_fd(),
            );
        }
    }
    // SO_NOSIGPIPE is used on Darwin; Linux supplies MSG_NOSIGNAL per send.
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(target_os = "macos")]
    let flags = 0;
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &message, flags) };
    ensure!(sent > 0, "UI send: {}", std::io::Error::last_os_error());
    socket.write_all(&header[sent as usize..])?;
    socket.write_all(&bytes)?;
    Ok(())
}

pub fn receive<T: DeserializeOwned>(socket: &mut UnixStream) -> Result<(T, Option<OwnedFd>)> {
    let mut header = [0u8; 4];
    let mut control = [0usize; 8];
    let mut iov = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: header.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    let read = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) };
    ensure!(
        read > 0,
        "UI channel closed: {}",
        std::io::Error::last_os_error()
    );
    // Adopt every delivered fd before validating framing, so malformed packets
    // cannot leak descriptors. Extra descriptors and control truncation fail.
    let mut descriptors = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&message);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let base = libc::CMSG_LEN(0) as usize;
                let count = ((*cmsg).cmsg_len as usize).saturating_sub(base)
                    / std::mem::size_of::<libc::c_int>();
                for i in 0..count {
                    let raw = std::ptr::read_unaligned(
                        libc::CMSG_DATA(cmsg).cast::<libc::c_int>().add(i),
                    );
                    let fd = OwnedFd::from_raw_fd(raw);
                    ensure!(
                        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) == 0,
                        "UI descriptor CLOEXEC failed"
                    );
                    descriptors.push(fd);
                }
            }
            cmsg = libc::CMSG_NXTHDR(&message, cmsg);
        }
    }
    ensure!(
        message.msg_flags & libc::MSG_CTRUNC == 0 && descriptors.len() <= 1,
        "invalid UI descriptor frame"
    );
    socket.read_exact(&mut header[read as usize..])?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(length > 0 && length <= LIMIT, "UI frame exceeds limit");
    let mut body = vec![0; length];
    socket.read_exact(&mut body).context("UI frame body")?;
    Ok((serde_json::from_slice(&body)?, descriptors.pop()))
}

pub fn configure(socket: &UnixStream) -> Result<()> {
    same_uid(socket)?;
    #[cfg(target_os = "macos")]
    unsafe {
        let yes: libc::c_int = 1;
        ensure!(
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&yes as *const libc::c_int).cast(),
                std::mem::size_of_val(&yes) as _
            ) == 0,
            "UI SO_NOSIGPIPE failed"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_handoff_keeps_original_bytes_after_path_replacement() -> Result<()> {
        use std::os::unix::fs::FileExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("received.txt");
        std::fs::write(&path, "original")?;
        let file = std::fs::File::open(&path)?;
        std::fs::remove_file(&path)?;
        std::fs::write(&path, "replacement")?;
        let (mut a, mut b) = UnixStream::pair()?;
        configure(&a)?;
        send(&mut a, &"open", Some(&file))?;
        let (command, descriptor): (String, _) = receive(&mut b)?;
        assert_eq!(command, "open");
        let received = std::fs::File::from(descriptor.context("descriptor")?);
        let mut bytes = [0u8; 8];
        received.read_exact_at(&mut bytes, 0)?;
        assert_eq!(&bytes, b"original");
        assert_ne!(std::fs::read(&path)?, bytes);
        Ok(())
    }
    #[test]
    fn frames_are_bounded_and_do_not_consume_the_next_descriptor() -> Result<()> {
        let (mut a, mut b) = UnixStream::pair()?;
        let file = tempfile::tempfile()?;
        send(&mut a, &1, None)?;
        send(&mut a, &2, Some(&file))?;
        assert!(receive::<u8>(&mut b)?.1.is_none());
        assert!(receive::<u8>(&mut b)?.1.is_some());
        assert!(send(&mut a, &"x".repeat(LIMIT + 1), None).is_err());
        a.write_all(&((LIMIT + 1) as u32).to_be_bytes())?;
        assert!(receive::<u8>(&mut b).is_err());
        Ok(())
    }
}
