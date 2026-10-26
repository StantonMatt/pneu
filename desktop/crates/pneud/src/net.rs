//! The daemon's wildcard TCP listener.
use std::io;
use tokio::net::TcpListener;

/// Bind [::] in a Tokio runtime, accepting IPv4 too on Windows.
pub async fn listen(port: u16) -> io::Result<TcpListener> {
    #[cfg(unix)]
    {
        TcpListener::bind(("::", port)).await
    }
    #[cfg(windows)]
    {
        windows_listener(port)
    }
}

#[cfg(windows)]
fn windows_listener(port: u16) -> io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::{net::SocketAddr, os::windows::io::AsRawSocket};
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOL_SOCKET, WSAGetLastError, setsockopt,
    };

    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    let exclusive = 1i32;
    // Both options must precede bind. Do not allow another process to share
    // this wildcard listener (Windows SO_REUSEADDR has different semantics).
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&exclusive as *const i32).cast(),
            std::mem::size_of_val(&exclusive) as i32,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    socket.set_nonblocking(true)?;
    socket
        .bind(&SocketAddr::from(([0u16; 8], port)).into())
        .map_err(bind_error)?;
    socket.listen(128)?;
    TcpListener::from_std(socket.into())
}

#[cfg(windows)]
fn bind_error(error: io::Error) -> io::Error {
    // An exclusive-address conflict can return WSAEACCES rather than
    // WSAEADDRINUSE. Normalize bind errors for the daemon's retry loop.
    // Reserved/excluded ports can also return WSAEACCES; retry those too.
    if error.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAEACCES) {
        io::Error::new(io::ErrorKind::AddrInUse, error)
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::IpAddr, time::Duration};

    #[cfg(windows)]
    #[test]
    fn bind_errors_normalize_exclusive_access_conflicts_and_preserve_other_errors() {
        let error = bind_error(io::Error::from_raw_os_error(10013));
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        let original = error
            .get_ref()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(original.raw_os_error(), Some(10013));
        let in_use = bind_error(io::Error::from_raw_os_error(10048));
        assert_eq!(in_use.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(in_use.raw_os_error(), Some(10048));
        let unrelated = bind_error(io::Error::from_raw_os_error(10049));
        assert_eq!(unrelated.raw_os_error(), Some(10049));
        assert_ne!(unrelated.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn wildcard_listener_accepts_ipv6() -> io::Result<()> {
        let listener = listen(0).await?;
        let port = listener.local_addr()?.port();
        let connect = tokio::net::TcpStream::connect(("::1", port));
        let (client, accepted) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(connect, listener.accept())
        })
        .await?;
        client?;
        assert!(accepted?.1.ip().is_loopback());
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn wildcard_listener_accepts_ipv4_and_ipv6() -> io::Result<()> {
        let listener = listen(0).await?;
        let port = listener.local_addr()?.port();
        for ip in ["127.0.0.1", "::1"] {
            let (client, accepted) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    tokio::net::TcpStream::connect((ip, port)),
                    listener.accept()
                )
            })
            .await?;
            let mut client = client?;
            let (mut accepted, peer) = accepted?;
            assert_eq!(peer.ip().to_canonical(), client.local_addr()?.ip());
            assert_eq!(peer.port(), client.local_addr()?.port());
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            accepted.write_all(b"ok").await?;
            let mut bytes = [0; 2];
            tokio::time::timeout(Duration::from_secs(3), async {
                client.read_exact(&mut bytes).await
            })
            .await??;
            assert_eq!(&bytes, b"ok");
        }
        Ok(())
    }

    #[tokio::test]
    async fn wildcard_listener_conflict_is_addr_in_use() -> io::Result<()> {
        let listener = listen(0).await?;
        assert_eq!(listener.local_addr()?.ip(), "::".parse::<IpAddr>().unwrap());
        let error = listen(listener.local_addr()?.port()).await.unwrap_err();
        eprintln!("listener conflict: {error:?}");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        Ok(())
    }
}
