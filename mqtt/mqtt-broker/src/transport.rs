use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, BufMut};
use core::mem::MaybeUninit;
use failure::{Fail, ResultExt};
use futures::stream::FuturesUnordered;
use native_tls::Identity;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    stream::Stream,
};
use tokio_native_tls::{TlsAcceptor, TlsStream};
use tracing::{debug, error, warn};

use crate::{Certificate, Error, ErrorKind, InitializeBrokerReason};

pub enum TransportBuilder<A> {
    Tcp(A),
    Tls(A, Identity),
}

impl<A> TransportBuilder<A>
where
    A: ToSocketAddrs,
{
    pub async fn build(self) -> Result<Transport, Error> {
        match self {
            TransportBuilder::Tcp(addr) => Transport::new_tcp(addr).await,
            TransportBuilder::Tls(addr, identity) => Transport::new_tls(addr, identity).await,
        }
    }
}

pub enum Transport {
    Tcp(TcpListener),
    Tls(TcpListener, TlsAcceptor),
}

impl Transport {
    pub async fn new_tcp<A>(addr: A) -> Result<Self, Error>
    where
        A: ToSocketAddrs,
    {
        let tcp = TcpListener::bind(addr)
            .await
            .context(ErrorKind::InitializeBroker(
                InitializeBrokerReason::BindServer,
            ))?;
        Ok(Transport::Tcp(tcp))
    }

    pub async fn new_tls<A>(addr: A, identity: Identity) -> Result<Self, Error>
    where
        A: ToSocketAddrs,
    {
        let acceptor = TlsAcceptor::from(
            native_tls::TlsAcceptor::builder(identity)
                .build()
                .context(ErrorKind::InitializeBroker(InitializeBrokerReason::Tls))?,
        );
        let tcp = TcpListener::bind(addr)
            .await
            .context(ErrorKind::InitializeBroker(
                InitializeBrokerReason::BindServer,
            ))?;
        Ok(Transport::Tls(tcp, acceptor))
    }

    pub fn incoming(&mut self) -> Incoming<'_> {
        match self {
            Self::Tcp(listener) => Incoming::Tcp(IncomingTcp::new(listener)),
            Self::Tls(listener, acceptor) => Incoming::Tls(IncomingTls::new(listener, acceptor)),
        }
    }

    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        let addr = match self {
            Self::Tcp(listener) => listener.local_addr(),
            Self::Tls(listener, _) => listener.local_addr(),
        };
        let addr = addr.context(ErrorKind::InitializeBroker(
            InitializeBrokerReason::ConnectionLocalAddress,
        ))?;
        Ok(addr)
    }
}

type HandshakeFuture =
    Pin<Box<dyn Future<Output = Result<TlsStream<TcpStream>, native_tls::Error>>>>;

pub enum Incoming<'a> {
    Tcp(IncomingTcp<'a>),
    Tls(IncomingTls<'a>),
}

impl Stream for Incoming<'_> {
    type Item = std::io::Result<StreamSelector>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            Self::Tcp(incoming) => Pin::new(incoming).poll_next(cx),
            Self::Tls(incoming) => Pin::new(incoming).poll_next(cx),
        }
    }
}

pub struct IncomingTcp<'a> {
    listener: &'a mut TcpListener,
}

impl<'a> IncomingTcp<'a> {
    fn new(listener: &'a mut TcpListener) -> Self {
        Self { listener }
    }
}

impl Stream for IncomingTcp<'_> {
    type Item = std::io::Result<StreamSelector>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.listener.poll_accept(cx) {
            Poll::Ready(Ok((tcp, _))) => match tcp.set_nodelay(true) {
                Ok(()) => {
                    debug!("TCP: Accepted connection from client");
                    Poll::Ready(Some(Ok(StreamSelector::Tcp(tcp))))
                }
                Err(err) => {
                    warn!(
                        "TCP: Dropping client because failed to setup TCP properties: {}",
                        err
                    );
                    Poll::Ready(Some(Err(err)))
                }
            },
            Poll::Ready(Err(err)) => {
                error!(
                    "TCP: Dropping client that failed to completely establish a TCP connection: {}",
                    err
                );
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct IncomingTls<'a> {
    listener: &'a mut TcpListener,
    acceptor: &'a TlsAcceptor,
    connections: FuturesUnordered<HandshakeFuture>,
}

impl<'a> IncomingTls<'a> {
    fn new(listener: &'a mut TcpListener, acceptor: &'a TlsAcceptor) -> Self {
        Self {
            listener,
            acceptor,
            connections: FuturesUnordered::default(),
        }
    }
}

impl Stream for IncomingTls<'_> {
    type Item = std::io::Result<StreamSelector>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _))) => match stream.set_nodelay(true) {
                    Ok(()) => {
                        let acceptor = self.acceptor.clone();
                        self.connections
                            .push(Box::pin(async move { acceptor.accept(stream).await }));
                    }
                    Err(err) => warn!(
                        "TCP: Dropping client because failed to setup TCP properties: {}",
                        err
                    ),
                },
                Poll::Ready(Err(err)) => warn!(
                    "TCP: Dropping client that failed to completely establish a TCP connection: {}",
                    err
                ),
                Poll::Pending => break,
            }
        }

        loop {
            if self.connections.is_empty() {
                return Poll::Pending;
            }

            match Pin::new(&mut self.connections).poll_next(cx) {
                Poll::Ready(Some(Ok(stream))) => {
                    debug!("TLS: Accepted connection from client");
                    return Poll::Ready(Some(Ok(StreamSelector::Tls(stream))));
                }

                Poll::Ready(Some(Err(err))) => warn!(
                    "TLS: Dropping client that failed to complete a TLS handshake: {}",
                    err
                ),

                Poll::Ready(None) => {
                    debug!("TLS: Shutting down web server");
                    return Poll::Ready(None);
                }

                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

pub enum StreamSelector {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl StreamSelector {
    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            StreamSelector::Tcp(stream) => stream.peer_addr(),
            StreamSelector::Tls(stream) => stream.get_ref().get_ref().get_ref().peer_addr(),
        }
    }
}

pub trait GetPeerCertificate {
    type Certificate;

    fn peer_certificate(&self) -> Result<Option<Self::Certificate>, Error>;
}

impl GetPeerCertificate for StreamSelector {
    type Certificate = Certificate;

    fn peer_certificate(&self) -> Result<Option<Self::Certificate>, Error> {
        match self {
            StreamSelector::Tcp(_) => Ok(None),
            StreamSelector::Tls(stream) => stream
                .get_ref()
                .peer_certificate()
                .and_then(|cert| {
                    cert.map(|cert| cert.to_der().map(Certificate::from))
                        .transpose()
                })
                .map_err(|e| e.context(ErrorKind::PeerCertificate).into()),
        }
    }
}

impl AsyncRead for StreamSelector {
    #[inline]
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [MaybeUninit<u8>]) -> bool {
        match self {
            StreamSelector::Tcp(stream) => stream.prepare_uninitialized_buffer(buf),
            StreamSelector::Tls(stream) => stream.prepare_uninitialized_buffer(buf),
        }
    }

    #[inline]
    fn poll_read_buf<B: BufMut>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_read_buf(cx, buf),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_read_buf(cx, buf),
        }
    }

    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for StreamSelector {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_write_buf<B: Buf>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut B,
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_write_buf(cx, buf),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_write_buf(cx, buf),
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            StreamSelector::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            StreamSelector::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}
