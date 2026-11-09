//! AF_VSOCK listener built directly on `libc` (§8.1): the guest serves only
//! the configured host-vsock endpoint and never opens a TCP listener in
//! production. On non-Linux targets the whole module is cfg-gated out so the
//! crate still type-checks.

use std::io;

/// Production listener abstraction: either a real vsock listener or the
/// test-only TCP loopback listener.
pub enum Listener {
    #[cfg(target_os = "linux")]
    Vsock(RawListener),
    Tcp(tokio::net::TcpListener),
}

impl Listener {
    /// Accept one association. Returns a boxed duplex stream.
    pub async fn accept(&self) -> io::Result<crate::server::BoxedStream> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(l) => {
                let fd = l.accept().await?;
                Ok(Box::new(VsockStream::from_fd(fd)?))
            }
            Self::Tcp(l) => {
                let (s, _) = l.accept().await?;
                Ok(Box::new(s))
            }
        }
    }
}

/// Raw AF_VSOCK socket fd wrapper (Linux only).
#[cfg(target_os = "linux")]
pub struct RawListener {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl RawListener {
    /// `socket(AF_VSOCK, SOCK_STREAM|SOCK_CLOEXEC, 0)` + bind(port, CID_ANY)
    /// + listen.
    pub fn bind(port: u32) -> io::Result<Self> {
        // SAFETY: plain socket creation; fd validated below.
        let fd = unsafe {
            libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a valid socket; sockaddr_vm layout is the kernel ABI.
        let addr = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: libc::VMADDR_CID_ANY,
            svm_zero: [0u8; 4],
        };
        let rc = unsafe {
            libc::bind(
                fd,
                std::ptr::from_ref(&addr).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_vm>() as u32,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            // SAFETY: matching close for the created fd.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // SAFETY: fd is valid.
        if unsafe { libc::listen(fd, 16) } != 0 {
            let e = io::Error::last_os_error();
            // SAFETY: matching close.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        // SAFETY: fd ownership transfers to OwnedFd exactly once.
        Ok(Self {
            fd: tokio::io::unix::AsyncFd::new(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })?,
        })
    }

    /// Await and accept one connection, returning its fd.
    pub async fn accept(&self) -> io::Result<std::os::fd::OwnedFd> {
        loop {
            let mut guard = self.fd.readable().await?;
            let accepted = guard.try_io(|inner| {
                // SAFETY: valid listener fd; a returned fd (>=0) is a new socket.
                let fd = unsafe {
                    libc::accept(
                        inner.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if fd < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(fd)
                }
            });
            match accepted {
                Ok(Ok(fd)) => {
                    // SAFETY: fresh accepted descriptor, owned exactly once.
                    return Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

/// Duplex vsock stream implementing tokio `AsyncRead`/`AsyncWrite` over a
/// nonblocking socket fd.
#[cfg(target_os = "linux")]
pub struct VsockStream {
    inner: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl VsockStream {
    /// Set nonblocking and wrap.
    pub fn from_fd(fd: std::os::fd::OwnedFd) -> io::Result<Self> {
        // SAFETY: valid socket fd; fcntl get/set pair.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            inner: tokio::io::unix::AsyncFd::new(fd)?,
        })
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncRead for VsockStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf,
    ) -> std::task::Poll<io::Result<()>> {
        loop {
            let mut guard = std::task::ready!(self.inner.poll_read_ready(cx))?;
            let dst = buf.initialize_unfilled();
            let n = guard.try_io(|inner| {
                // SAFETY: writing into ReadBuf's spare capacity only.
                let n =
                    unsafe { libc::read(inner.as_raw_fd(), dst.as_mut_ptr().cast(), dst.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match n {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return std::task::Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return std::task::Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncWrite for VsockStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        loop {
            let mut guard = std::task::ready!(self.inner.poll_write_ready(cx))?;
            let r = guard.try_io(|inner| {
                // SAFETY: read-only borrow of `buf`.
                let n = unsafe { libc::write(inner.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match r {
                Ok(Ok(n)) => return std::task::Poll::Ready(Ok(n)),
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return std::task::Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Ok(())) // sockets are unbuffered here
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        // SAFETY: valid fd; SHUT_RDWR is an advisory shutdown.
        unsafe {
            libc::shutdown(self.inner.get_ref().as_raw_fd(), libc::SHUT_RDWR);
        }
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};

/// Bind the production vsock listener (Linux); unsupported elsewhere.
pub fn bind_vsock(port: u32) -> io::Result<Listener> {
    #[cfg(target_os = "linux")]
    {
        Ok(Listener::Vsock(RawListener::bind(port)?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = port;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "vsock requires linux",
        ))
    }
}

/// TEST-ONLY TCP loopback listener (requires `--test-tcp` + env gate in
/// `main`; never used in production paths).
pub async fn bind_test_tcp(port: u32) -> io::Result<Listener> {
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port as u16)).await?;
    Ok(Listener::Tcp(l))
}
