use anyhow::Context;
use std::collections::VecDeque;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time;
use std::{io, mem, ptr};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT};
use windows::Win32::Networking::WinSock::{
    ADDRESS_FAMILY, AF_UNIX, AcceptEx, INVALID_SOCKET, SEND_RECV_FLAGS, SO_RCVTIMEO, SO_SNDTIMEO,
    SOCK_STREAM, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR, SOL_SOCKET, SOMAXCONN,
    WSA_FLAG_OVERLAPPED, WSA_IO_PENDING, WSABUF, WSARecv, WSASend, WSASocketW, accept, bind,
    closesocket, connect, listen, recv, send, setsockopt,
};
use windows::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, GetQueuedCompletionStatusEx,
    OVERLAPPED, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};
use windows::Win32::System::Threading::INFINITE;
use windows::core::PSTR;

use crate::utils::LogIfErr;

const UNIX_ADDR_LEN: u32 = mem::size_of::<SOCKADDR_UN>() as u32;

// Adapted from Rust's standard library (std::sys::net::connection::socket::windows, commit 6ba0ce4)
mod winsock {
    use std::mem;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::{AcqRel, Relaxed};

    use windows::Win32::Networking::WinSock::{WSACleanup, WSADATA, WSAStartup};

    static WSA_INITIALIZED: AtomicBool = AtomicBool::new(false);

    /// Checks whether the Windows socket interface has been started already, and
    /// if not, starts it.
    #[inline]
    pub fn init() {
        if !WSA_INITIALIZED.load(Relaxed) {
            wsa_startup();
        }
    }

    #[cold]
    fn wsa_startup() {
        unsafe {
            let mut data: WSADATA = mem::zeroed();
            let ret = WSAStartup(
                0x202, // version 2.2
                &mut data,
            );
            assert_eq!(ret, 0);
            if WSA_INITIALIZED.swap(true, AcqRel) {
                // If another thread raced with us and called WSAStartup first then call
                // WSACleanup so it's as though WSAStartup was only called once.
                WSACleanup();
            }
        }
    }

    #[allow(unused)]
    pub fn cleanup() {
        // We don't need to call WSACleanup here because exiting the process will cause
        // the OS to clean everything for us, which is faster than doing it manually.
        // See #141799.
    }
}

pub struct OverlappedContext {
    pub overlapped: Box<OVERLAPPED>,
    pub buffer: Option<Vec<u8>>,
    pub flags: Box<u32>,
}

pub struct UnixListener {
    pub socket: UnixDomainSocket,
}

unsafe impl Send for UnixListener {}

impl UnixListener {
    pub fn bind(socket_path: &Path) -> io::Result<Self> {
        winsock::init();

        let server_socket = UnixDomainSocket::new()?;
        server_socket.bind(socket_path)?;
        server_socket.listen(SOMAXCONN as i32)?;

        Ok(Self {
            socket: server_socket,
        })
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// Upon completion, the local and remote addresses are written into the `buffer` of the
    /// returned `UnixStream`'s `overlapped_context`. The layout of this buffer is determined by
    /// AcceptEx; use GetAcceptExSockaddrs to parse the addresses.
    pub unsafe fn accept_overlapped(&self) -> io::Result<UnixStream> {
        // I'm not 100% sure why we need at least this Vec len, but it's just double the len used
        // in AcceptEx (double I assume because there's both the local and remote addresses)
        let mut socket_addr = vec![0u8; ((UNIX_ADDR_LEN + 16) * 2) as usize];
        let mut overlapped = Box::new(OVERLAPPED::default());
        let client_socket = unsafe {
            self.socket
                .accept_overlapped(&mut socket_addr, &mut overlapped)
        }?;

        Ok(UnixStream {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped,
                buffer: Some(socket_addr),
                flags: Box::new(0),
            },
        })
    }

    pub fn accept(&self) -> io::Result<UnixStream> {
        let client_socket = self.socket.accept(None, None)?;

        Ok(UnixStream {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped: Box::new(OVERLAPPED::default()),
                buffer: None,
                flags: Box::new(0),
            },
        })
    }

    pub fn token(&self) -> usize {
        self.socket.0.0
    }
}

pub struct UnixStream {
    pub socket: UnixDomainSocket,
    pub overlapped_context: OverlappedContext,
}

unsafe impl Send for UnixStream {}

impl UnixStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        winsock::init();

        let client_socket = UnixDomainSocket::new()?;
        client_socket.connect(path)?;

        Ok(Self {
            socket: client_socket,
            overlapped_context: OverlappedContext {
                overlapped: Box::new(OVERLAPPED::default()),
                buffer: None,
                flags: Box::new(0),
            },
        })
    }

    fn set_socket_timeout(&self, option: i32, timeout: time::Duration) -> io::Result<()> {
        let timeout_ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
        let timeout_bytes = timeout_ms.to_ne_bytes();
        let result = unsafe { setsockopt(self.socket.0, SOL_SOCKET, option, Some(&timeout_bytes)) };
        if result == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Bounds synchronous reads so an idle IPC client cannot hold daemon shutdown open forever.
    pub fn set_read_timeout(&self, timeout: time::Duration) -> io::Result<()> {
        self.set_socket_timeout(SO_RCVTIMEO, timeout)
    }

    /// Bounds synchronous writes so a client that stops reading responses cannot hold shutdown open.
    pub fn set_write_timeout(&self, timeout: time::Duration) -> io::Result<()> {
        self.set_socket_timeout(SO_SNDTIMEO, timeout)
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// The return value indicates the immediate number of bytes transferred. A non-zero value
    /// indicates synchronous completion. Otherwise, the operation is pending and will complete
    /// asynchronously, and the number of bytes transferred must be retrieved via the completion
    /// notification mechanism.
    ///
    /// NOTE: This takes ownership of the input buffer to avoid race conditions
    pub unsafe fn read_overlapped(&mut self, outputbuffer: Vec<u8>) -> io::Result<u32> {
        let context = &mut self.overlapped_context;

        // Reset flags between I/O operations
        *context.flags = 0;

        // Here is where we take ownership of the buffer
        context.buffer = Some(outputbuffer);

        unsafe {
            self.socket.read_overlapped(
                context.buffer.as_mut().unwrap(),
                &mut context.overlapped,
                &mut context.flags,
            )
        }
    }

    /// # Safety
    ///
    /// Performs an asynchronous I/O operation using an `OVERLAPPED` struct owned by the
    /// `UnixListener` or `UnixStream`. To uphold memory safety, ensure the following:
    ///
    /// - Only one overlapped operation should be in flight at a time per struct. Running
    ///   concurrent operations on the same struct is undefined behavior.
    /// - This struct must not be dropped until the operation has completed. Dropping it early may
    ///   lead to use-after-free errors.
    ///
    /// The return value indicates the immediate number of bytes transferred. A non-zero value
    /// indicates synchronous completion. Otherwise, the operation is pending and will complete
    /// asynchronously, and the number of bytes transferred must be retrieved via the completion
    /// notification mechanism.
    pub unsafe fn write_overlapped(&mut self, inputbuffer: &[u8]) -> io::Result<u32> {
        let context = &mut self.overlapped_context;

        // Reset flags between I/O operations
        *context.flags = 0;

        unsafe {
            self.socket
                .write_overlapped(inputbuffer, &mut context.overlapped, *context.flags)
        }
    }

    pub fn token(&self) -> usize {
        self.socket.0.0
    }

    pub fn take_overlapped_buffer(&mut self) -> Option<Vec<u8>> {
        self.overlapped_context.buffer.take()
    }
}

// These delegate to their respective methods for &UnixStream
impl io::Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &*self, buf)
    }
}

impl io::Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut &*self)
    }
}

// These allow use with non-mut references (mut isn't necessary for this type)
impl io::Read for &UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket
            .read(buf, SEND_RECV_FLAGS::default())
            .map(|bytes| bytes as usize)
    }
}

impl io::Write for &UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket
            .write(buf, SEND_RECV_FLAGS::default())
            .map(|bytes| bytes as usize)
    }

    // This is a no-op in Rust's standard library's UnixStream too
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct UnixDomainSocket(SOCKET);

impl UnixDomainSocket {
    pub fn new() -> io::Result<Self> {
        let socket = unsafe {
            WSASocketW(
                AF_UNIX as i32,
                SOCK_STREAM.0,
                0,
                None,
                0,
                WSA_FLAG_OVERLAPPED,
            )
        }?;

        Ok(Self(socket))
    }

    pub fn bind(&self, path: &Path) -> io::Result<()> {
        let sockaddr_un = sockaddr_un(path)?;

        let iresult = unsafe {
            bind(
                self.0,
                ptr::addr_of!(sockaddr_un) as *const SOCKADDR,
                mem::size_of_val(&sockaddr_un) as i32,
            )
        };
        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    pub fn connect(&self, path: &Path) -> io::Result<()> {
        let sockaddr_un = sockaddr_un(path)?;

        if unsafe {
            connect(
                self.0,
                ptr::addr_of!(sockaddr_un) as *const SOCKADDR,
                mem::size_of_val(&sockaddr_un) as i32,
            )
        } == SOCKET_ERROR
        {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        if unsafe { listen(self.0, backlog) } == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(())
    }

    pub fn accept(
        &self,
        addr: Option<*mut SOCKADDR>,
        addrlen: Option<&mut i32>,
    ) -> io::Result<UnixDomainSocket> {
        let socket = unsafe { accept(self.0, addr, addrlen.map(|mut_ref| mut_ref as *mut _)) }?;

        Ok(UnixDomainSocket(socket))
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn accept_overlapped(
        &self,
        lpoutputbuffer: &mut [u8],
        lpoverlapped: &mut OVERLAPPED,
    ) -> io::Result<UnixDomainSocket> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        let client_socket = UnixDomainSocket::new()?;
        let mut bytes_transferred = 0;

        if !unsafe {
            AcceptEx(
                self.0,
                client_socket.0,
                lpoutputbuffer as *mut _ as *mut _,
                0,
                // We add 16 to the address length because MSDN says so
                UNIX_ADDR_LEN + 16,
                UNIX_ADDR_LEN + 16,
                &mut bytes_transferred,
                lpoverlapped,
            )
        }
        .as_bool()
        {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        };

        Ok(client_socket)
    }

    pub fn read(&self, buf: &mut [u8], flags: SEND_RECV_FLAGS) -> io::Result<u32> {
        let bytes_transferred = unsafe { recv(self.0, buf, flags) };

        if bytes_transferred == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(bytes_transferred as u32)
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn read_overlapped(
        &self,
        lpoutputbuffer: &mut [u8],
        lpoverlapped: &mut OVERLAPPED,
        lpflags: &mut u32,
    ) -> io::Result<u32> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        let lpbuffers = WSABUF {
            len: lpoutputbuffer.len() as u32,
            buf: PSTR(lpoutputbuffer.as_mut_ptr()),
        };
        let mut bytes_transferred = 0;

        // Note that we set lpnumberofbytesrecvd to a non-null pointer even though MSDN recommends
        // setting it to null when lpoverlapped is non-null. We do this anyways because the field
        // is still updated if the operation completes immediately, allowing us to indicate so.
        let iresult = unsafe {
            WSARecv(
                self.0,
                &[lpbuffers],
                Some(&mut bytes_transferred),
                lpflags,
                Some(lpoverlapped),
                None,
            )
        };

        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        }

        Ok(bytes_transferred)
    }

    pub fn write(&self, buf: &[u8], flags: SEND_RECV_FLAGS) -> io::Result<u32> {
        let bytes_transferred = unsafe { send(self.0, buf, flags) };

        if bytes_transferred == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();
            return Err(last_error);
        }

        Ok(bytes_transferred as u32)
    }

    /// Performs an overlapped (asynchronous) I/O operation. Completion must be handled via
    /// external mechanisms such as an I/O completion port.
    ///
    /// # Safety
    ///
    /// The caller must ensure that all references passed to this function remain valid and are not
    /// not reused until the operation completes. Refer to MSDN for more details on overlapped I/O.
    pub unsafe fn write_overlapped(
        &self,
        lpinputbuffer: &[u8],
        lpoverlapped: &mut OVERLAPPED,
        lpflags: u32,
    ) -> io::Result<u32> {
        // Zero out unused OVERLAPPED struct fields (as per MSDN recommendation)
        *lpoverlapped = OVERLAPPED {
            hEvent: lpoverlapped.hEvent,
            ..Default::default()
        };

        // WSABUF requires a mut ptr to the buffer, but WSASend shouldn't mutate anything.
        // It should be safe to cast a const ptr to a mut ptr as a workaround.
        let lpbuffers = WSABUF {
            len: lpinputbuffer.len() as u32,
            buf: PSTR(lpinputbuffer.as_ptr() as *mut _),
        };
        let mut bytes_transferred = 0;

        // Note that we set lpnumberofbytessent to a non-null pointer even though MSDN recommends
        // setting it to null when lpoverlapped is non-null. We do this anyways because the field
        // is still updated if the operation completes immediately, allowing us to indicate so.
        let iresult = unsafe {
            WSASend(
                self.0,
                &[lpbuffers],
                Some(&mut bytes_transferred),
                lpflags,
                Some(lpoverlapped),
                None,
            )
        };

        if iresult == SOCKET_ERROR {
            let last_error = io::Error::last_os_error();

            if last_error.raw_os_error() != Some(WSA_IO_PENDING.0) {
                return Err(last_error);
            }
        }

        Ok(bytes_transferred)
    }

    pub fn to_handle(&self) -> HANDLE {
        HANDLE(self.0.0 as _)
    }
}

impl Drop for UnixDomainSocket {
    fn drop(&mut self) {
        let iresult = unsafe { closesocket(self.0) };
        if iresult != 0 {
            error!(
                "could not close unix domain socket {:?}: {}",
                self.0,
                io::Error::last_os_error()
            )
        }
    }
}

fn sockaddr_un(path: &Path) -> io::Result<SOCKADDR_UN> {
    let mut sun_path = [0i8; 108];
    let path_bytes = path.as_os_str().as_encoded_bytes();

    if path_bytes.len() > sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }

    for (i, byte) in path_bytes.iter().enumerate() {
        sun_path[i] = *byte as i8;
    }

    Ok(SOCKADDR_UN {
        sun_family: ADDRESS_FAMILY(AF_UNIX),
        sun_path,
    })
}

#[derive(Debug)]
pub struct CompletionPort(HANDLE);

unsafe impl Send for CompletionPort {}

impl CompletionPort {
    pub fn new(threads: u32) -> io::Result<Self> {
        let iocp_handle =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, None, 0, threads) }?;

        Ok(Self(iocp_handle))
    }

    pub fn associate_handle(&self, handle: HANDLE, token: usize) -> io::Result<()> {
        // This just returns the HANDLE of the existing iocp, so we can ignore the return value
        let _ = unsafe { CreateIoCompletionPort(handle, Some(self.0), token, 0) }?;

        Ok(())
    }

    pub fn poll_single(
        &self,
        timeout: Option<time::Duration>,
        entry: &mut OVERLAPPED_ENTRY,
    ) -> io::Result<()> {
        let mut bytes_transferred = 0u32;
        let mut completion_key = 0usize;
        let mut lpoverlapped: *mut OVERLAPPED = ptr::null_mut();

        let timeout_ms = match timeout {
            Some(duration) => duration.as_millis() as u32,
            None => INFINITE,
        };

        unsafe {
            GetQueuedCompletionStatus(
                self.0,
                &mut bytes_transferred,
                &mut completion_key,
                &mut lpoverlapped,
                timeout_ms,
            )
        }?;

        *entry = OVERLAPPED_ENTRY {
            lpCompletionKey: completion_key,
            lpOverlapped: lpoverlapped,
            Internal: 0,
            dwNumberOfBytesTransferred: bytes_transferred,
        };

        Ok(())
    }

    pub fn poll_many(
        &self,
        timeout: Option<time::Duration>,
        entries: &mut [OVERLAPPED_ENTRY],
    ) -> io::Result<u32> {
        let mut num_entries_removed = 0u32;

        let timeout_ms = match timeout {
            Some(duration) => duration.as_millis() as u32,
            None => INFINITE,
        };

        let result = unsafe {
            GetQueuedCompletionStatusEx(
                self.0,
                entries,
                &mut num_entries_removed,
                timeout_ms,
                false,
            )
        };
        if result.is_err() {
            let err = io::Error::last_os_error();
            if timeout.is_some() && err.raw_os_error() == Some(WAIT_TIMEOUT.0 as i32) {
                return Ok(0);
            }
            return Err(err);
        }

        Ok(num_entries_removed)
    }
}

impl Drop for CompletionPort {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }
            .with_context(|| format!("could not close i/o completion port {:?}", self.0))
            .log_if_err();
    }
}

// Like AsRawHandle, but specifically for windows-rs' HANDLE type
pub trait AsWin32Handle {
    fn as_win32_handle(&self) -> HANDLE;
}

impl AsWin32Handle for CompletionPort {
    fn as_win32_handle(&self) -> HANDLE {
        self.0
    }
}

pub trait AsWin32Socket {
    fn as_win32_socket(&self) -> SOCKET;
}

impl AsWin32Socket for UnixDomainSocket {
    fn as_win32_socket(&self) -> SOCKET {
        self.0
    }
}

pub struct UnixStreamSink {
    iocp_handle: HANDLE,
    stop: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
}

impl UnixStreamSink {
    const MAX_COMPLETION_EVENTS: usize = 8;
    const BUFFER_POOL_PRUNE_INTERVAL: time::Duration = time::Duration::from_secs(600);
    const STOP_POLL_INTERVAL: time::Duration = time::Duration::from_millis(250);
    const BUFFER_SIZE: usize = 32768;

    // Currently, tokens/keys are just the values of the corresponding SOCKETs, which is why the value
    // below (INVALID_SOCKET) should work as a special key that won't interfere with others.
    const STOP_PACKET_KEY: usize = INVALID_SOCKET.0;

    fn queue_accept(
        listener: &UnixListener,
        port: &CompletionPort,
        streams_queue: &mut VecDeque<(usize, Box<UnixStream>)>,
    ) -> anyhow::Result<()> {
        let stream =
            Box::new(unsafe { listener.accept_overlapped() }.context("could not accept stream")?);
        let token = stream.token();

        // Own the OVERLAPPED before the next fallible operation. If associating the accepted
        // socket with the IOCP fails, exceptional-exit cleanup can still cancel and drain the
        // pending AcceptEx request through the listener completion key.
        streams_queue.push_back((token, stream));
        let stream = streams_queue
            .back()
            .map(|(_, stream)| stream)
            .context("could not get queued accept stream")?;
        port.associate_handle(stream.socket.to_handle(), token)
            .context("could not associate stream with iocp")
    }

    pub fn new(
        socket_path: &Path,
        mut callback: impl FnMut(&[u8], u32) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let listener = UnixListener::bind(socket_path).context("could not bind listener")?;
        let listener_key = listener.token();

        let port = CompletionPort::new(2).context("could not create iocp")?;
        port.associate_handle(listener.socket.to_handle(), listener_key)
            .context("could not associate listener with iocp")?;

        let iocp_handle = port.as_win32_handle();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let thread_handle = thread::spawn(move || {
            debug!("entering unix stream sink thread");

            let mut entries = vec![OVERLAPPED_ENTRY::default(); Self::MAX_COMPLETION_EVENTS];
            let mut buffer_pool = VecDeque::<Vec<u8>>::new();
            let mut streams_queue = VecDeque::<(usize, Box<UnixStream>)>::new();
            let mut last_buffer_pool_prune = time::Instant::now();
            let mut next_dequeued_entry = 0usize;
            let mut num_dequeued_entries = 0usize;

            // Keep the ownership state outside the fallible run closure. Returning Err or
            // unwinding from the user callback therefore cannot drop listener/stream OVERLAPPED
            // storage before the common cleanup epilogue has canceled and drained it.
            let run_result = catch_unwind(AssertUnwindSafe(|| -> anyhow::Result<()> {
                Self::queue_accept(&listener, &port, &mut streams_queue)?;
                let mut should_cleanup = false;

                loop {
                    if last_buffer_pool_prune.elapsed() > Self::BUFFER_POOL_PRUNE_INTERVAL {
                        debug!("pruning buffer pool for unix stream sink");
                        buffer_pool.truncate(1);
                        last_buffer_pool_prune = time::Instant::now();
                    }

                    // Use a finite poll so the shared stop flag is still an escape path if
                    // posting the dedicated stop packet ever fails.
                    num_dequeued_entries =
                        port.poll_many(Some(Self::STOP_POLL_INTERVAL), &mut entries)
                            .context("could not poll with iocp")? as usize;
                    next_dequeued_entry = 0;

                    while next_dequeued_entry < num_dequeued_entries {
                        let entry = &entries[next_dequeued_entry];
                        let completion_key = entry.lpCompletionKey;
                        let bytes_transferred = entry.dwNumberOfBytesTransferred;

                        // GetQueuedCompletionStatusEx has already removed this entry from the
                        // port. Advance before running fallible code or the user callback so the
                        // cleanup epilogue only accounts for entries later in this same batch.
                        next_dequeued_entry += 1;

                        if completion_key == listener_key {
                            // Stream has been accepted; ready to read. The accepted stream is the
                            // queue tail because only one AcceptEx is outstanding at a time.
                            let read_result = {
                                let stream = streams_queue
                                    .back_mut()
                                    .map(|(_, stream)| stream)
                                    .context("could not get stream")?;
                                let outputbuffer = buffer_pool.pop_front().unwrap_or_else(|| {
                                    debug!("creating new buffer for unix stream sink");
                                    vec![0u8; Self::BUFFER_SIZE]
                                });
                                unsafe { stream.read_overlapped(outputbuffer) }
                            };

                            if let Err(err) = read_result {
                                // read_overlapped only returns Err for a non-pending WSARecv
                                // failure. No completion will arrive for this stream, so remove it
                                // from the pending queue before entering the common drain path.
                                let _ = streams_queue.pop_back();
                                return Err(err).context("could not read with stream");
                            }

                            Self::queue_accept(&listener, &port, &mut streams_queue)?;
                        } else if completion_key != Self::STOP_PACKET_KEY {
                            // Stream has been read; ready to process. Its completion has already
                            // been dequeued, so the local stream is safe to drop even if callback
                            // panics; all other pending streams remain owned by streams_queue.
                            let position = streams_queue
                                .iter()
                                .position(|(token, _)| *token == completion_key)
                                .context("could not find stream")?;
                            let mut stream = streams_queue
                                .remove(position)
                                .context("could not remove stream from queue")?
                                .1;
                            let outputbuffer = stream
                                .take_overlapped_buffer()
                                .context("unix stream's buffer is None")?;

                            callback(&outputbuffer, bytes_transferred);
                            buffer_pool.push_back(outputbuffer);
                        } else {
                            should_cleanup = true;
                        }
                    }

                    next_dequeued_entry = 0;
                    num_dequeued_entries = 0;

                    if should_cleanup || stop_clone.load(Ordering::Acquire) {
                        break;
                    }
                }

                Ok(())
            }));

            // This epilogue is intentionally unconditional. It runs after success, Result errors,
            // and callback panics before the panic is resumed.
            let cleanup_result = Self::cleanup(
                listener,
                listener_key,
                port,
                entries,
                next_dequeued_entry,
                num_dequeued_entries,
                streams_queue,
            );
            if let Err(err) = cleanup_result {
                error!("could not safely clean up unix stream sink worker: {err:#}");
            }

            match run_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => error!("unix stream sink worker failed: {err:#}"),
                Err(payload) => {
                    error!(
                        "unix stream sink callback panicked after pending i/o lifetime was resolved"
                    );
                    resume_unwind(payload);
                }
            }

            debug!("exiting unix stream sink thread");
        });

        Ok(Self {
            iocp_handle,
            stop,
            thread_handle: Some(thread_handle),
        })
    }

    fn cleanup(
        listener: UnixListener,
        listener_key: usize,
        port: CompletionPort,
        mut entries: Vec<OVERLAPPED_ENTRY>,
        next_dequeued_entry: usize,
        num_dequeued_entries: usize,
        mut streams_queue: VecDeque<(usize, Box<UnixStream>)>,
    ) -> anyhow::Result<()> {
        // A fallible operation or callback panic can stop processing partway through a batch
        // returned by GetQueuedCompletionStatusEx. Those entries have already left the port, so
        // account for them before waiting for any completions that remain queued in the kernel.
        for entry in entries[next_dequeued_entry..num_dequeued_entries].iter() {
            let key = entry.lpCompletionKey;
            if key == Self::STOP_PACKET_KEY {
                continue;
            }
            if key == listener_key {
                let _ = streams_queue.pop_back();
                continue;
            }

            let position = streams_queue.iter().position(|(token, _)| *token == key);
            let Some(position) = position else {
                return Err(Self::preserve_pending_io_on_cleanup_failure(
                    listener,
                    port,
                    streams_queue,
                    anyhow::Error::msg(format!(
                        "could not find completion key {key} in partially processed iocp batch"
                    )),
                ));
            };
            let _ = streams_queue.remove(position);
        }

        // Cancel any pending I/O operations on the listener
        let listener_handle = listener.socket.to_handle();
        unsafe { CancelIoEx(listener_handle, None) }
            .with_context(|| format!("could not cancel i/o for listener {listener_handle:?}"))
            .log_if_err();

        // Cancel any pending I/O operations on each stream
        // NOTE: A stream may not have any pending I/O operations if it is still in
        // the accept stage, and CancelIoEx will return an error in those cases.
        for (_, stream) in streams_queue.iter() {
            let stream_handle = stream.socket.to_handle();
            unsafe { CancelIoEx(stream_handle, None) }
                .with_context(|| format!("could not cancel i/o for stream {stream_handle:?}"))
                .log_if_err();
        }

        // Canceled OVERLAPPED requests still own their buffers until completion is dequeued. There
        // is intentionally no timeout here: timing out and dropping streams_queue would free memory
        // that the kernel may still reference.
        while !streams_queue.is_empty() {
            let num_removed = match port.poll_many(None, &mut entries) {
                Ok(num_removed) => num_removed,
                Err(err) => {
                    return Err(Self::preserve_pending_io_on_cleanup_failure(
                        listener,
                        port,
                        streams_queue,
                        anyhow::Error::new(err),
                    ));
                }
            };

            for entry in entries[..num_removed as usize].iter() {
                let key = entry.lpCompletionKey;
                if key == Self::STOP_PACKET_KEY {
                    // A stop packet can race the shared stop-flag fallback and arrive after the
                    // worker has already entered cleanup. It owns no OVERLAPPED storage.
                    continue;
                }
                if key == listener_key {
                    let _ = streams_queue.pop_back();
                    continue;
                }

                let position = streams_queue.iter().position(|(token, _)| *token == key);
                let Some(position) = position else {
                    return Err(Self::preserve_pending_io_on_cleanup_failure(
                        listener,
                        port,
                        streams_queue,
                        anyhow::Error::msg(format!(
                            "could not find completion key {key} while draining canceled i/o"
                        )),
                    ));
                };
                let _ = streams_queue.remove(position);
            }
        }

        Ok(())
    }

    fn preserve_pending_io_on_cleanup_failure(
        listener: UnixListener,
        port: CompletionPort,
        streams_queue: VecDeque<(usize, Box<UnixStream>)>,
        err: anyhow::Error,
    ) -> anyhow::Error {
        // Safety wins over cleanup completeness on a catastrophic IOCP-drain failure. If we can no
        // longer prove that canceled operations completed, intentionally keep every object backing
        // their OVERLAPPED pointers alive until process exit rather than risk use-after-free.
        let preserved = anyhow::Error::msg(format!(
            "could not prove pending i/o completion; preserving listener, iocp, and pending \
OVERLAPPED storage until process exit: {err:#}"
        ));
        mem::forget(streams_queue);
        mem::forget(listener);
        mem::forget(port);
        preserved
    }
}

impl Drop for UnixStreamSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Err(err) =
            unsafe { PostQueuedCompletionStatus(self.iocp_handle, 0, Self::STOP_PACKET_KEY, None) }
        {
            // The worker also polls the shared stop flag at a bounded interval, so a failed stop
            // packet cannot orphan the JoinHandle.
            error!(
                "could not post stop packet to iocp {:?} for unix stream sink: {err:#}",
                self.iocp_handle
            );
        }

        match self.thread_handle.take() {
            Some(handle) => {
                if let Err(err) = handle.join() {
                    error!("could not join unix stream sink thread handle: {err:?}");
                }
            }
            None => error!("could not take unix stream sink thread handle"),
        }
    }
}

pub fn write_to_unix_socket(socket_path: &Path, message: &mut [u8]) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path).context("could not connect stream")?;
    stream
        .write_all(message)
        .context("could not write all with stream")?;

    Ok(())
}
