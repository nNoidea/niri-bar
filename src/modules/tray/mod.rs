//! System tray (StatusNotifierItem): D-Bus watcher/service, proactive discovery,
//! per-item client/menu tracking, pure protocol parsing, and GTK widgets.
//! Only the service/widget entry points are re-exported.

mod item_client;
mod protocol;
mod service;
mod types;
mod widget;

pub use service::TrayService;
pub use widget::TrayWidget;

#[cfg(test)]
pub(crate) mod test_socket {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug)]
    pub(crate) struct TestUnixSocket {
        stream: tokio::net::UnixStream,
        bytes_read: Option<Arc<AtomicUsize>>,
    }

    impl TestUnixSocket {
        pub(crate) fn new(stream: tokio::net::UnixStream) -> Self {
            Self {
                stream,
                bytes_read: None,
            }
        }

        pub(super) fn with_counter(stream: tokio::net::UnixStream, bytes_read: Arc<AtomicUsize>) -> Self {
            Self {
                stream,
                bytes_read: Some(bytes_read),
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct TestUnixReader {
        read: tokio::io::ReadHalf<tokio::net::UnixStream>,
        bytes_read: Option<Arc<AtomicUsize>>,
    }

    #[derive(Debug)]
    pub(crate) struct TestUnixWriter(tokio::io::WriteHalf<tokio::net::UnixStream>);

    impl zbus::connection::socket::Socket for TestUnixSocket {
        type ReadHalf = TestUnixReader;
        type WriteHalf = TestUnixWriter;

        fn split(self) -> zbus::connection::socket::Split<Self::ReadHalf, Self::WriteHalf> {
            let (read, write) = tokio::io::split(self.stream);
            zbus::connection::socket::Split::new(
                TestUnixReader {
                    read,
                    bytes_read: self.bytes_read,
                },
                TestUnixWriter(write),
            )
        }
    }

    #[zbus::export::async_trait::async_trait]
    impl zbus::connection::socket::ReadHalf for TestUnixReader {
        async fn recvmsg(&mut self, buffer: &mut [u8]) -> std::io::Result<(usize, Vec<std::os::fd::OwnedFd>)> {
            use tokio::io::AsyncReadExt;
            let n = self.read.read(buffer).await?;
            if let Some(ref counter) = self.bytes_read {
                counter.fetch_add(n, Ordering::Relaxed);
            }
            Ok((n, Vec::new()))
        }
    }

    #[zbus::export::async_trait::async_trait]
    impl zbus::connection::socket::WriteHalf for TestUnixWriter {
        async fn sendmsg(&mut self, buffer: &[u8], _fds: &[std::os::fd::BorrowedFd<'_>]) -> std::io::Result<usize> {
            use tokio::io::AsyncWriteExt;
            self.0.write(buffer).await
        }

        async fn flush(&mut self) -> std::io::Result<()> {
            use tokio::io::AsyncWriteExt;
            self.0.flush().await
        }

        async fn close(&mut self) -> std::io::Result<()> {
            use tokio::io::AsyncWriteExt;
            self.0.shutdown().await
        }
    }
}
