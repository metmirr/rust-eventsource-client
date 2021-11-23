use std::{
    fmt::{self, Debug, Display, Formatter},
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{ready, Stream};
use hyper::{
    body::HttpBody,
    client::{connect::Connect, ResponseFuture},
    header::HeaderMap,
    header::HeaderValue,
    Body, Request, StatusCode, Uri,
};
#[cfg(feature = "rustls")]
use hyper_rustls::HttpsConnector as RustlsConnector;
use log::{debug, info, trace, warn};
use pin_project::pin_project;
use tokio::time::Sleep;

use crate::{event_parser::EventParser, Event};

use super::config::ReconnectOptions;
use super::error::{Error, Result};

pub use hyper::client::HttpConnector;
#[cfg(feature = "rustls")]
pub type HttpsConnector = RustlsConnector<HttpConnector>;

/*
 * TODO remove debug output
 * TODO specify list of stati to not retry (e.g. 204)
 */

pub struct ClientBuilder {
    url: Uri,
    headers: HeaderMap,
    reconnect_opts: ReconnectOptions,
    last_event_id: String,
}

impl ClientBuilder {
    /// Set a HTTP header on the SSE request.
    pub fn header(mut self, key: &'static str, value: &str) -> Result<ClientBuilder> {
        let value = value.parse().map_err(|e| Error::HttpRequest(Box::new(e)))?;
        self.headers.insert(key, value);
        Ok(self)
    }

    /// Set the last event id for a stream when it is created. If it is set, it will be sent to the
    /// server in case it can replay missed events.
    pub fn last_event_id(mut self, last_event_id: String) -> ClientBuilder {
        self.last_event_id = last_event_id;
        self
    }

    /// Configure the client's reconnect behaviour according to the supplied
    /// [`ReconnectOptions`].
    ///
    /// [`ReconnectOptions`]: struct.ReconnectOptions.html
    pub fn reconnect(mut self, opts: ReconnectOptions) -> ClientBuilder {
        self.reconnect_opts = opts;
        self
    }

    fn build_with_conn<C>(self, conn: C) -> Client<C>
    where
        C: Connect + Clone,
    {
        Client {
            http: hyper::Client::builder().build(conn),
            request_props: RequestProps {
                url: self.url,
                headers: self.headers,
                reconnect_opts: self.reconnect_opts,
            },
            last_event_id: self.last_event_id,
        }
    }

    pub fn build_http(self) -> Client<HttpConnector> {
        self.build_with_conn(HttpConnector::new())
    }

    #[cfg(feature = "rustls")]
    pub fn build(self) -> Client<HttpsConnector> {
        let conn = HttpsConnector::with_native_roots();
        self.build_with_conn(conn)
    }

    pub fn build_with_http_client<C>(self, http: hyper::Client<C>) -> Client<C> {
        Client {
            http,
            request_props: RequestProps {
                url: self.url,
                headers: self.headers,
                reconnect_opts: self.reconnect_opts,
            },
            last_event_id: self.last_event_id,
        }
    }
}

#[derive(Clone)]
struct RequestProps {
    url: Uri,
    headers: HeaderMap,
    reconnect_opts: ReconnectOptions,
}

/// Client that connects to a server using the Server-Sent Events protocol
/// and consumes the event stream indefinitely.
pub struct Client<C> {
    http: hyper::Client<C>,
    request_props: RequestProps,
    last_event_id: String,
}

impl Client<()> {
    /// Construct a new `Client` (via a [`ClientBuilder`]). This will not
    /// perform any network activity until [`.stream()`] is called.
    ///
    /// [`ClientBuilder`]: struct.ClientBuilder.html
    /// [`.stream()`]: #method.stream
    pub fn for_url(url: &str) -> Result<ClientBuilder> {
        let url = url.parse().map_err(|e| Error::HttpRequest(Box::new(e)))?;

        let mut header_map = HeaderMap::new();
        header_map.insert("Accept", HeaderValue::from_static("text/event-stream"));
        header_map.insert("Cache-Control", HeaderValue::from_static("no-cache"));

        Ok(ClientBuilder {
            url,
            headers: header_map,
            reconnect_opts: ReconnectOptions::default(),
            last_event_id: String::new(),
        })
    }
}

pub type EventStream<C> = ReconnectingRequest<C>;

impl<C> Client<C> {
    /// Connect to the server and begin consuming the stream. Produces a
    /// [`Stream`] of [`Event`](crate::Event)s wrapped in [`Result`].
    ///
    /// Do not use the stream after it returned an error!
    ///
    /// After the first successful connection, the stream will
    /// reconnect for retryable errors.
    pub fn stream(&self) -> EventStream<C>
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        ReconnectingRequest::new(
            self.http.clone(),
            self.request_props.clone(),
            self.last_event_id.clone(),
        )
    }
}

#[must_use = "streams do nothing unless polled"]
#[pin_project]
pub struct ReconnectingRequest<C> {
    http: hyper::Client<C>,
    props: RequestProps,
    #[pin]
    state: State,
    next_reconnect_delay: Duration,
    event_parser: EventParser,
    last_event_id: String,
}

#[allow(clippy::large_enum_variant)] // false positive
#[pin_project(project = StateProj)]
enum State {
    New,
    Connecting {
        retry: bool,
        #[pin]
        resp: ResponseFuture,
    },
    Connected(#[pin] hyper::Body),
    WaitingToReconnect(#[pin] Sleep),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::New => "new",
            State::Connecting { retry: false, .. } => "connecting(no-retry)",
            State::Connecting { retry: true, .. } => "connecting(retry)",
            State::Connected(_) => "connected",
            State::WaitingToReconnect(_) => "waiting-to-reconnect",
        }
    }
}

impl Debug for State {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl<C> ReconnectingRequest<C> {
    fn new(
        http: hyper::Client<C>,
        props: RequestProps,
        last_event_id: String,
    ) -> ReconnectingRequest<C> {
        let reconnect_delay = props.reconnect_opts.delay;
        ReconnectingRequest {
            props,
            http,
            state: State::New,
            next_reconnect_delay: reconnect_delay,
            event_parser: EventParser::new(),
            last_event_id,
        }
    }
}

impl<C> ReconnectingRequest<C> {
    fn send_request(&self) -> Result<ResponseFuture>
    where
        C: Connect + Clone + Send + Sync + 'static,
    {
        let mut request = Request::get(&self.props.url);
        *request.headers_mut().unwrap() = self.props.headers.clone();
        if !self.last_event_id.is_empty() {
            request.headers_mut().unwrap().insert(
                "last-event-id",
                HeaderValue::from_str(&self.last_event_id.clone()).unwrap(),
            );
        }
        let request = request
            .body(Body::empty())
            .map_err(|e| Error::HttpRequest(Box::new(e)))?;
        Ok(self.http.request(request))
    }

    fn backoff(mut self: Pin<&mut Self>) -> Duration {
        let delay = self.next_reconnect_delay;
        let this = self.as_mut().project();
        let mut next_reconnect_delay = std::cmp::min(
            this.props.reconnect_opts.delay_max,
            *this.next_reconnect_delay * this.props.reconnect_opts.backoff_factor,
        );
        mem::swap(this.next_reconnect_delay, &mut next_reconnect_delay);
        delay
    }

    fn reset_backoff(self: Pin<&mut Self>) {
        let mut delay = self.props.reconnect_opts.delay;
        let this = self.project();
        mem::swap(this.next_reconnect_delay, &mut delay);
    }
}

fn delay(dur: Duration, description: &str) -> Sleep {
    info!("Waiting {:?} before {}", dur, description);
    tokio::time::sleep(dur)
}

#[derive(Debug)]
struct StatusError {
    status: StatusCode,
}

impl Display for StatusError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid status code: {}", self.status)
    }
}

impl std::error::Error for StatusError {}

impl<C> Stream for ReconnectingRequest<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    type Item = Result<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        trace!("ReconnectingRequest::poll({:?})", &self.state);

        loop {
            let this = self.as_mut().project();
            if let Some(event) = this.event_parser.get_event() {
                if !event.id.is_empty() {
                    *this.last_event_id = String::from_utf8(event.id.clone()).unwrap();
                }
                return Poll::Ready(Some(Ok(event)));
            }

            trace!("ReconnectingRequest::poll loop({:?})", &this.state);

            let state = this.state.project();
            let new_state = match state {
                // New immediately transitions to Connecting, and exists only
                // to ensure that we only connect when polled.
                StateProj::New => {
                    let resp = match self.send_request() {
                        Err(e) => return Poll::Ready(Some(Err(e))),
                        Ok(r) => r,
                    };
                    State::Connecting {
                        resp,
                        retry: self.props.reconnect_opts.retry_initial,
                    }
                }
                StateProj::Connecting { retry, resp } => match ready!(resp.poll(cx)) {
                    Ok(resp) => {
                        debug!("HTTP response: {:#?}", resp);

                        if !resp.status().is_success() {
                            let e = StatusError {
                                status: resp.status(),
                            };
                            return Poll::Ready(Some(Err(Error::HttpRequest(Box::new(e)))));
                        }

                        self.as_mut().reset_backoff();
                        State::Connected(resp.into_body())
                    }
                    Err(e) => {
                        warn!("request returned an error: {}", e);
                        if !*retry {
                            return Poll::Ready(Some(Err(Error::HttpStream(Box::new(e)))));
                        }
                        State::WaitingToReconnect(delay(self.as_mut().backoff(), "retrying"))
                    }
                },
                StateProj::Connected(body) => match ready!(body.poll_data(cx)) {
                    Some(Ok(result)) => {
                        this.event_parser.process_bytes(result)?;
                        continue;
                    }
                    _ if self.props.reconnect_opts.reconnect => {
                        State::WaitingToReconnect(delay(self.as_mut().backoff(), "reconnecting"))
                    }
                    Some(Err(e)) => return Poll::Ready(Some(Err(Error::HttpStream(Box::new(e))))),
                    None => return Poll::Ready(None),
                },
                StateProj::WaitingToReconnect(delay) => {
                    ready!(delay.poll(cx));
                    info!("Reconnecting");
                    let resp = self.send_request()?;
                    State::Connecting { retry: true, resp }
                }
            };
            self.as_mut().project().state.set(new_state);
        }
    }
}
