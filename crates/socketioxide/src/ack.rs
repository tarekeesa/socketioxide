//! Acknowledgement related types and functions.
//!
//! Here is the main type:
//!
//! - [`AckStream`]: A [`Stream`]/[`Future`] of data received from the client.
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use engineioxide::sid::Sid;
use futures_core::{FusedFuture, FusedStream, Future, Stream};
use futures_util::stream::FuturesUnordered;
use serde::de::DeserializeOwned;
use tokio::{sync::oneshot::Receiver, time::Timeout};

use crate::{
    adapter::Adapter,
    errors::{AckError, SocketError},
    extract::SocketRef,
    packet::Packet,
    parser::Parser,
};
use socketioxide_core::{parser::Parse, Value};
pub(crate) type AckResult<T> = Result<T, AckError>;

pin_project_lite::pin_project! {
    /// A [`Future`] of [`AckResponse`] received from the client with its corresponding [`Sid`].
    /// It is used internally by [`AckStream`] and **should not** be used directly.
    pub struct AckResultWithId<T> {
        id: Sid,
        #[pin]
        result: Timeout<Receiver<AckResult<T>>>,
    }
}

impl<T> Future for AckResultWithId<T> {
    type Output = (Sid, AckResult<T>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let project = self.project();
        match project.result.poll(cx) {
            Poll::Ready(v) => {
                let v = match v {
                    Ok(Ok(Ok(v))) => Ok(v),
                    Ok(Ok(Err(e))) => Err(e),
                    Ok(Err(_)) => Err(AckError::Socket(SocketError::Closed)),
                    Err(_) => Err(AckError::Timeout),
                };
                Poll::Ready((*project.id, v))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pin_project_lite::pin_project! {
    /// A [`Stream`]/[`Future`] of data received from the client.
    ///
    /// It can be used in two ways:
    /// * As a [`Stream`]: It will yield all the ack responses with their corresponding socket id
    /// received from the client. It can useful when broadcasting to multiple sockets and therefore expecting
    /// more than one acknowledgement.
    /// * As a [`Future`]: It will yield the first ack response received from the client.
    /// Useful when expecting only one acknowledgement.
    ///
    /// If the client didn't respond before the timeout, the [`AckStream`] will yield
    /// an [`AckError::Timeout`]. If the data sent by the client is not deserializable as `T`,
    /// an [`AckError::Decode`] will be yielded.
    ///
    /// An [`AckStream`] can be created from:
    /// * The [`SocketRef::emit_with_ack`] method, in this case there will be only one ack response.
    /// * The [`Operator::emit_with_ack`] method, in this case there will be as many ack response
    /// as there are selected sockets.
    /// * The [`SocketIo::emit_with_ack`] method, in this case there will be as many ack response
    /// as there are sockets in the namespace.
    ///
    /// [`SocketRef::emit_with_ack`]: crate::extract::SocketRef#method.emit_with_ack
    /// [`Operator::emit_with_ack`]: crate::operators::Operators#method.emit_with_ack
    /// [`SocketIo::emit_with_ack`]: crate::SocketIo#method.emit_with_ack
    ///
    /// # Example
    /// ```rust
    /// # use socketioxide::extract::SocketRef;
    /// # use socketioxide::ack::AckStream;
    /// # use futures_util::StreamExt;
    /// # use socketioxide::SocketIo;
    /// let (svc, io) = SocketIo::new_svc();
    /// io.ns("/test", move |socket: SocketRef| async move {
    ///     // We wait for the acknowledgement of the first emit (only one in this case)
    ///     let ack = socket.emit_with_ack::<_, String>("test", "test").unwrap().await;
    ///     println!("Ack: {:?}", ack);
    ///
    ///     // We apply the `for_each` StreamExt fn to the AckStream
    ///     socket.broadcast().emit_with_ack::<_, String>("test", "test")
    ///         .unwrap()
    ///         .for_each(|(id, ack)| async move { println!("Ack: {} {:?}", id, ack); }).await;
    /// });
    /// ```
    #[must_use = "futures and streams do nothing unless you `.await` or poll them"]
    pub struct AckStream<T> {
        #[pin]
        inner: AckInnerStream,
        parser: Parser,
        _marker: std::marker::PhantomData<T>,
    }
}

pin_project_lite::pin_project! {
    #[allow(missing_docs)]
    #[project = InnerProj]
    /// An internal stream used by [`AckStream`]. It should not be used directly except when implementing the
    /// [`Adapter`](crate::adapter::Adapter) trait.
    pub enum AckInnerStream {
        Stream {
            #[pin]
            rxs: FuturesUnordered<AckResultWithId<Value>>,
        },

        Fut {
            #[pin]
            rx: AckResultWithId<Value>,
            polled: bool,
        },
    }
}

// ==== impl AckInnerStream ====

impl AckInnerStream {
    /// Creates a new [`AckInnerStream`] from a [`Packet`] and a list of sockets.
    /// The [`Packet`] is sent to all the sockets and the [`AckInnerStream`] will wait
    /// for an acknowledgement from each socket.
    ///
    /// The [`AckInnerStream`] will wait for the default timeout specified in the config
    /// (5s by default) if no custom timeout is specified.
    pub fn broadcast<A: Adapter>(
        packet: Packet,
        sockets: Vec<SocketRef<A>>,
        duration: Option<Duration>,
    ) -> Self {
        let rxs = FuturesUnordered::new();

        if sockets.is_empty() {
            return AckInnerStream::Stream { rxs };
        }

        let duration =
            duration.unwrap_or_else(|| sockets.first().unwrap().get_io().config().ack_timeout);
        for socket in sockets {
            let rx = socket.send_with_ack(packet.clone());
            rxs.push(AckResultWithId {
                result: tokio::time::timeout(duration, rx),
                id: socket.id,
            });
        }
        AckInnerStream::Stream { rxs }
    }

    /// Creates a new [`AckInnerStream`] from a [`oneshot::Receiver`](tokio) corresponding to the acknowledgement
    /// of a single socket.
    pub fn send(rx: Receiver<AckResult<Value>>, duration: Duration, id: Sid) -> Self {
        AckInnerStream::Fut {
            polled: false,
            rx: AckResultWithId {
                id,
                result: tokio::time::timeout(duration, rx),
            },
        }
    }
}

impl Stream for AckInnerStream {
    type Item = (Sid, AckResult<Value>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use InnerProj::*;

        match self.project() {
            Fut { polled, .. } if *polled => Poll::Ready(None),
            Stream { rxs } => rxs.poll_next(cx),
            Fut { rx, polled } => match rx.poll(cx) {
                Poll::Ready(val) => {
                    *polled = true;
                    Poll::Ready(Some(val))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        use AckInnerStream::*;
        match self {
            Stream { rxs, .. } => rxs.size_hint(),
            Fut { .. } => (1, Some(1)),
        }
    }
}

impl FusedStream for AckInnerStream {
    fn is_terminated(&self) -> bool {
        use AckInnerStream::*;
        match self {
            Stream { rxs, .. } => rxs.is_terminated(),
            Fut { polled, .. } => *polled,
        }
    }
}

impl Future for AckInnerStream {
    type Output = AckResult<Value>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.as_mut().poll_next(cx) {
            Poll::Ready(Some(v)) => Poll::Ready(v.1),
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                unreachable!("stream should at least yield 1 value")
            }
        }
    }
}

impl FusedFuture for AckInnerStream {
    fn is_terminated(&self) -> bool {
        use AckInnerStream::*;
        match self {
            Stream { rxs, .. } => rxs.is_terminated(),
            Fut { polled, .. } => *polled,
        }
    }
}

// ==== impl AckStream ====
impl<T> AckStream<T> {
    pub(crate) fn new(inner: AckInnerStream, parser: Parser) -> Self {
        AckStream {
            inner,
            parser,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: DeserializeOwned> Stream for AckStream<T> {
    type Item = (Sid, AckResult<T>);

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let parser = self.parser;
        self.project()
            .inner
            .poll_next(cx)
            .map(|v| v.map(|(s, v)| (s, map_ack_response(v, parser))))
    }

    #[inline(always)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T: DeserializeOwned> FusedStream for AckStream<T> {
    #[inline(always)]
    fn is_terminated(&self) -> bool {
        FusedStream::is_terminated(&self.inner)
    }
}

impl<T: DeserializeOwned> Future for AckStream<T> {
    type Output = AckResult<T>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let parser = self.parser;
        self.project()
            .inner
            .poll(cx)
            .map(|v| map_ack_response(v, parser))
    }
}

impl<T: DeserializeOwned> FusedFuture for AckStream<T> {
    #[inline(always)]
    fn is_terminated(&self) -> bool {
        FusedFuture::is_terminated(&self.inner)
    }
}

fn map_ack_response<T: DeserializeOwned>(ack: AckResult<Value>, parser: Parser) -> AckResult<T> {
    ack.and_then(|mut v| parser.decode_value(&mut v, false).map_err(AckError::Decode))
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use engineioxide::sid::Sid;
    use futures_util::StreamExt;
    use socketioxide_parser_common::CommonParser;

    use crate::{adapter::LocalAdapter, ns::Namespace, socket::Socket};

    use super::*;

    fn create_socket() -> Arc<Socket<LocalAdapter>> {
        let sid = Sid::new();
        let ns = Namespace::<LocalAdapter>::new_dummy([sid]);
        let socket = Socket::new_dummy(sid, ns);
        socket.into()
    }
    fn get_packet() -> Packet {
        use socketioxide_core::parser::Parse;
        let parser = Parser::default();
        Packet::event("/", parser.encode_value(&"test", Some("test")).unwrap())
    }
    fn value(data: impl serde::Serialize) -> Value {
        CommonParser.encode_value(&data, None).unwrap()
    }
    impl<T: DeserializeOwned> From<AckInnerStream> for AckStream<T> {
        fn from(val: AckInnerStream) -> Self {
            Self::new(val, Parser::default())
        }
    }

    #[tokio::test]
    async fn broadcast_ack() {
        let socket = create_socket();
        let socket2 = create_socket();
        let mut packet = get_packet();
        packet.inner.set_ack_id(1);
        let socks = vec![socket.clone().into(), socket2.clone().into()];
        let stream: AckStream<String> = AckInnerStream::broadcast(packet, socks, None).into();

        let res_packet = Packet::ack("test", value("test"), 1);
        socket.recv(res_packet.inner.clone()).unwrap();
        socket2.recv(res_packet.inner).unwrap();

        futures_util::pin_mut!(stream);

        assert!(stream.next().await.unwrap().1.is_ok());
        assert!(stream.next().await.unwrap().1.is_ok());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn ack_stream() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        tx.send(Ok(value("test"))).unwrap();

        futures_util::pin_mut!(stream);

        assert_eq!(stream.next().await.unwrap().1.unwrap(), "test".to_string());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn ack_fut() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        tx.send(Ok(value("test"))).unwrap();

        assert_eq!(stream.await.unwrap(), "test".to_string());
    }

    #[tokio::test]
    async fn broadcast_ack_with_deserialize_error() {
        let socket = create_socket();
        let socket2 = create_socket();
        let mut packet = get_packet();
        packet.inner.set_ack_id(1);
        let socks = vec![socket.clone().into(), socket2.clone().into()];
        let stream: AckStream<String> = AckInnerStream::broadcast(packet, socks, None).into();

        let res_packet = Packet::ack("test", value(132), 1);
        socket.recv(res_packet.inner.clone()).unwrap();
        socket2.recv(res_packet.inner).unwrap();

        futures_util::pin_mut!(stream);

        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Decode(_)
        ));
        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Decode(_)
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn ack_stream_with_deserialize_error() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        tx.send(Ok(value(true))).unwrap();
        assert_eq!(stream.size_hint().0, 1);
        assert_eq!(stream.size_hint().1.unwrap(), 1);

        futures_util::pin_mut!(stream);

        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Decode(_)
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn ack_fut_with_deserialize_error() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        tx.send(Ok(value(true))).unwrap();

        assert!(matches!(stream.await.unwrap_err(), AckError::Decode(_)));
    }

    #[tokio::test]
    async fn broadcast_ack_with_closed_socket() {
        let socket = create_socket();
        let socket2 = create_socket();
        let mut packet = get_packet();
        packet.inner.set_ack_id(1);
        let socks = vec![socket.clone().into(), socket2.clone().into()];
        let stream: AckStream<String> = AckInnerStream::broadcast(packet, socks, None).into();

        let res_packet = Packet::ack("test", value("test"), 1);
        socket.clone().recv(res_packet.inner.clone()).unwrap();

        futures_util::pin_mut!(stream);

        let (id, ack) = stream.next().await.unwrap();
        assert_eq!(id, socket.id);
        assert!(ack.is_ok());

        let sid = socket2.id;
        socket2.disconnect().unwrap();
        let (id, ack) = stream.next().await.unwrap();
        assert_eq!(id, sid);
        assert!(matches!(ack, Err(AckError::Socket(SocketError::Closed))));
        assert!(stream.next().await.is_none());
    }
    #[tokio::test]
    async fn ack_stream_with_closed_socket() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        drop(tx);

        futures_util::pin_mut!(stream);

        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Socket(SocketError::Closed)
        ));
    }

    #[tokio::test]
    async fn ack_fut_with_closed_socket() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_secs(1), sid).into();
        drop(tx);

        assert!(matches!(
            stream.await.unwrap_err(),
            AckError::Socket(SocketError::Closed)
        ));
    }

    #[tokio::test]
    async fn broadcast_ack_with_timeout() {
        let socket = create_socket();
        let socket2 = create_socket();
        let mut packet = get_packet();
        packet.inner.set_ack_id(1);
        let socks = vec![socket.clone().into(), socket2.clone().into()];
        let stream: AckStream<String> =
            AckInnerStream::broadcast(packet, socks, Some(Duration::from_millis(10))).into();

        socket
            .recv(Packet::ack("test", value("test"), 1).inner)
            .unwrap();

        futures_util::pin_mut!(stream);

        assert!(stream.next().await.unwrap().1.is_ok());
        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Timeout
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn ack_stream_with_timeout() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_millis(10), sid).into();

        futures_util::pin_mut!(stream);

        assert!(matches!(
            stream.next().await.unwrap().1.unwrap_err(),
            AckError::Timeout
        ));
    }

    #[tokio::test]
    async fn ack_fut_with_timeout() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let sid = Sid::new();
        let stream: AckStream<String> =
            AckInnerStream::send(rx, Duration::from_millis(10), sid).into();

        assert!(matches!(stream.await.unwrap_err(), AckError::Timeout));
    }
}
