//! Websockets Filters

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::io::{self};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{body, header};
use crate::filter::{Filter, One};
use crate::reject::Rejection;
use crate::reply::{Reply, Response};
use futures::{future, FutureExt, Stream, Sink};
use headers::{Connection, HeaderMapExt, SecWebsocketAccept, SecWebsocketKey, Upgrade};
use http;
use tungstenite::protocol::{self, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

/// Creates a Websocket Filter.
///
/// The yielded `Ws` is used to finish the websocket upgrade.
///
/// # Note
///
/// This filter combines multiple filters internally, so you don't need them:
///
/// - Method must be `GET`
/// - Header `connection` must be `upgrade`
/// - Header `upgrade` must be `websocket`
/// - Header `sec-websocket-version` must be `13`
/// - Header `sec-websocket-key` must be set.
///
/// If the filters are met, yields a `Ws`. Calling `Ws::on_upgrade` will
/// return a reply with:
///
/// - Status of `101 Switching Protocols`
/// - Header `connection: upgrade`
/// - Header `upgrade: websocket`
/// - Header `sec-websocket-accept` with the hash value of the received key.
pub fn ws() -> impl Filter<Extract = One<Ws>, Error = Rejection> + Copy {
    let connection_has_upgrade = header::header2()
        .and_then(|conn: ::headers::Connection| {
            if conn.contains("upgrade") {
                future::ok(())
            } else {
                future::err(crate::reject::known(MissingConnectionUpgrade))
            }
        })
        .untuple_one();

    crate::get()
        .and(connection_has_upgrade)
        .and(header::exact_ignore_case("upgrade", "websocket"))
        .and(header::exact("sec-websocket-version", "13"))
        //.and(header::exact2(Upgrade::websocket()))
        //.and(header::exact2(SecWebsocketVersion::V13))
        .and(header::header2::<SecWebsocketKey>())
        .and(body::body())
        .map(move |key: SecWebsocketKey, body: ::hyper::Body| Ws {
            body,
            config: None,
            key,
        })
}

/// Extracted by the [`ws`](ws) filter, and used to finish an upgrade.
pub struct Ws {
    body: ::hyper::Body,
    config: Option<WebSocketConfig>,
    key: SecWebsocketKey,
}

impl Ws {
    /// Finish the upgrade, passing a function to handle the `WebSocket`.
    ///
    /// The passed function must return a `Future`.
    pub fn on_upgrade<F, U>(self, func: F) -> impl Reply
    where
        F: FnOnce(WebSocket) -> U + Send + 'static,
        U: Future<Output = ()> + Send + 'static,
    {
        WsReply {
            ws: self,
            on_upgrade: func,
        }
    }

    // config

    /// Set the size of the internal message send queue.
    pub fn max_send_queue(mut self, max: usize) -> Self {
        self.config
            .get_or_insert_with(WebSocketConfig::default)
            .max_send_queue = Some(max);
        self
    }

    /// Set the maximum message size (defaults to 64 megabytes)
    pub fn max_message_size(mut self, max: usize) -> Self {
        self.config
            .get_or_insert_with(WebSocketConfig::default)
            .max_message_size = Some(max);
        self
    }
}

impl fmt::Debug for Ws {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ws").finish()
    }
}

#[allow(missing_debug_implementations)]
struct WsReply<F> {
    ws: Ws,
    on_upgrade: F,
}

impl<F, U> Reply for WsReply<F>
where
    F: FnOnce(WebSocket) -> U + Send + 'static,
    U: Future<Output = ()> + Send + 'static,
{
    fn into_response(self) -> Response {
        let on_upgrade = self.on_upgrade;
        let config = self.ws.config;
        let fut = self
            .ws
            .body
            .on_upgrade()
            .then(move |upgraded| {
                log::trace!("websocket upgrade complete");
                let socket = WebSocketStream::from_raw_socket(upgraded.unwrap(), protocol::Role::Server, config);
                socket
            })
            .then(move |socket| {
                on_upgrade(WebSocket { inner: socket })
            })
            .map(|_result| {
                /*
                if let Err(err) = result {
                    log::debug!("ws upgrade error: {}", err);
                }
                */
            });
        ::tokio::task::spawn(fut);

        let mut res = http::Response::default();

        *res.status_mut() = http::StatusCode::SWITCHING_PROTOCOLS;

        res.headers_mut().typed_insert(Connection::upgrade());
        res.headers_mut().typed_insert(Upgrade::websocket());
        res.headers_mut()
            .typed_insert(SecWebsocketAccept::from(self.ws.key));

        res
    }
}


/// A websocket `Stream` and `Sink`, provided to `ws` filters.
pub struct WebSocket {
    inner: WebSocketStream<hyper::upgrade::Upgraded>,
}

/*
fn cvt<T>(r: tungstenite::error::Result<T>, err_message: &str) -> Poll<Result<T, crate::Error>> {
    match r {
        Ok(v) => Poll::Ready(Ok(v)),
        Err(tungstenite::Error::Io(ref e)) if e.kind() == io::ErrorKind::WouldBlock => {
            Poll::Pending
        }
        Err(e) => {
            log::debug!("{} {}", err_message, e);
            Poll::Ready(Err(crate::Error::new(e)))
        }
    }
}
*/

impl WebSocket {
    pub(crate) async fn from_raw_socket(
        upgraded: hyper::upgrade::Upgraded,
        role: protocol::Role,
        config: Option<protocol::WebSocketConfig>
    ) -> Self {
        WebSocketStream::from_raw_socket(
            upgraded,
            role,
            config
        ).map(|inner| {
            WebSocket { inner }
        }).await
    }

    fn with_context<F, R>(&mut self, ctx: Option<&mut Context<'_>>, f: F) -> R
    where
        F: FnOnce(Pin<&mut WebSocketStream<hyper::upgrade::Upgraded>>, Option<&mut Context<'_>>) -> R,
    {
        f(Pin::new(&mut self.inner), ctx)
    }

    /// Gracefully close this websocket.
    pub async fn close(mut self) -> Result<(), crate::Error> {
        future::poll_fn(|cx| Pin::new(&mut self).poll_close(cx)).await
    }
}

impl Stream for WebSocket {
    type Item = Result<Message, crate::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let msg = match (*self).with_context(Some(cx), |s, cx| s.poll_next(cx.unwrap())) {
                Poll::Ready(Some(Ok(item))) => item,
                Poll::Ready(Some(Err(::tungstenite::Error::Io(ref err))))
                    if err.kind() == io::ErrorKind::WouldBlock =>
                {
                    return Poll::Pending;
                }
                Poll::Ready(Some(Err(::tungstenite::Error::ConnectionClosed))) => {
                    log::trace!("websocket closed");
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    log::debug!("websocket poll error: {}", e);
                    return Poll::Ready(Some(Err(crate::Error::new(e))));
                }
                Poll::Ready(None) => {
                    log::trace!("websocket closed");
                    return Poll::Ready(None);
                },
                Poll::Pending => {
                    return Poll::Pending;
                }
            };

            match msg {
                msg @ protocol::Message::Text(..)
                | msg @ protocol::Message::Binary(..)
                | msg @ protocol::Message::Close(..)
                | msg @ protocol::Message::Ping(..) => {
                    return Poll::Ready(Some(Ok(Message { inner: msg })));
                }
                protocol::Message::Pong(payload) => {
                    log::trace!("websocket client pong: {:?}", payload);
                }
            }
        }
    }
}

impl Sink<Message> for WebSocket {
    type Error = crate::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (*self).with_context(Some(cx), |s, cx| {
            match s.poll_ready(cx.unwrap()) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(crate::Error::new(e))),
                Poll::Pending => Poll::Pending
            }
            // cvt(s.write_pending(), "websocket poll_ready error")
        })
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        if let protocol::Message::Ping(..) = item.inner {
            // warp doesn't yet expose a way to construct a `Ping` message,
            // so the only way this could is if the user is forwarding the
            // received `Ping`s straight back.
            //
            // tungstenite already auto-reponds to `Ping`s with a `Pong`,
            // so this just prevents accidentally sending extra pings.
            return Ok(());
        }

        match self.with_context(None, |s, _| s.start_send(item.inner)) {
            Ok(()) => Ok(()),
            // Err(::tungstenite::Error::SendQueueFull(inner)) => {
            //     log::debug!("websocket send queue full");
            //     Err(::tungstenite::Error::SendQueueFull(inner))
            // }
            Err(::tungstenite::Error::Io(ref err)) if err.kind() == io::ErrorKind::WouldBlock => {
                // the message was accepted and queued
                // isn't an error.
                Ok(())
            }
            Err(e) => {
                log::debug!("websocket start_send error: {}", e);
                Err(crate::Error::new(e))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.with_context(Some(cx), |s, cx| {
            //cvt(s.write_pending(), "websocket poll_flush error")
            match s.poll_flush(cx.unwrap()) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(crate::Error::new(e))),
                Poll::Pending => Poll::Pending
            }
        })
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        match self.with_context(Some(cx), |s, cx| s.poll_close(cx.unwrap())) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(err)) => {
                log::debug!("websocket close error: {}", err);
                Poll::Ready(Err(crate::Error::new(err)))
            },
            Poll::Pending => Poll::Pending
        }
    }
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocket").finish()
    }
}


/// A WebSocket message.
///
/// Only repesents Text and Binary messages.
///
/// This will likely become a `non-exhaustive` enum in the future, once that
/// language feature has stabilized.
#[derive(Eq, PartialEq, Clone)]
pub struct Message {
    inner: protocol::Message,
}

impl Message {
    /// Construct a new Text `Message`.
    pub fn text<S: Into<String>>(s: S) -> Message {
        Message {
            inner: protocol::Message::text(s),
        }
    }

    /// Construct a new Binary `Message`.
    pub fn binary<V: Into<Vec<u8>>>(v: V) -> Message {
        Message {
            inner: protocol::Message::binary(v),
        }
    }

    /// Construct a new Ping `Message`.
    pub fn ping<V: Into<Vec<u8>>>(v: V) -> Message {
        Message {
            inner: protocol::Message::Ping(v.into()),
        }
    }

    /// Construct the default Close `Message`.
    pub fn close() -> Message {
        Message {
            inner: protocol::Message::Close(None),
        }
    }

    /// Construct a Close `Message` with a code and reason.
    pub fn close_with(code: impl Into<u16>, reason: impl Into<Cow<'static, str>>) -> Message {
        Message {
            inner: protocol::Message::Close(Some(protocol::frame::CloseFrame {
                code: protocol::frame::coding::CloseCode::from(code.into()),
                reason: reason.into(),
            })),
        }
    }

    /// Returns true if this message is a Text message.
    pub fn is_text(&self) -> bool {
        self.inner.is_text()
    }

    /// Returns true if this message is a Binary message.
    pub fn is_binary(&self) -> bool {
        self.inner.is_binary()
    }

    /// Returns true if this message a is a Close message.
    pub fn is_close(&self) -> bool {
        self.inner.is_close()
    }

    /// Returns true if this message is a Ping message.
    pub fn is_ping(&self) -> bool {
        self.inner.is_ping()
    }

    /// Try to get a reference to the string text, if this is a Text message.
    pub fn to_str(&self) -> Result<&str, ()> {
        match self.inner {
            protocol::Message::Text(ref s) => Ok(s),
            _ => Err(()),
        }
    }

    /// Return the bytes of this message, if the message can contain data.
    pub fn as_bytes(&self) -> &[u8] {
        match self.inner {
            protocol::Message::Text(ref s) => s.as_bytes(),
            protocol::Message::Binary(ref v) => v,
            protocol::Message::Ping(ref v) => v,
            protocol::Message::Pong(ref v) => v,
            protocol::Message::Close(_) => &[],
        }
    }

    /// Destructure this message into binary data.
    pub fn into_bytes(self) -> Vec<u8> {
        self.inner.into_data()
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

impl Into<Vec<u8>> for Message {
    fn into(self) -> Vec<u8> {
        self.into_bytes()
    }
}

// ===== Rejections =====

#[derive(Debug)]
pub(crate) struct MissingConnectionUpgrade;

impl ::std::fmt::Display for MissingConnectionUpgrade {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "Connection header did not include 'upgrade'")
    }
}

impl ::std::error::Error for MissingConnectionUpgrade {}
