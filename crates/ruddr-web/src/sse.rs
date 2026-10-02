//! Server-sent event responses with a bounded queue. A reader that falls
//! more than [`QUEUE_LIMIT`] bytes behind is closed; EventSource reconnects
//! and the stream starts over with a reset. Dropping the response body (the
//! client went away) runs the producer's cleanup exactly once.

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::sync::Notify;

pub const QUEUE_LIMIT: usize = 48 * 1024 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(15);

type Cleanup = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    chunks: VecDeque<Bytes>,
    queued: usize,
    closed: bool,
    waker: Option<Waker>,
    cleanup: Option<Cleanup>,
}

struct Shared {
    queue: Mutex<Queue>,
    closed: Notify,
    limit: usize,
}

/// The producer's handle. Cloning shares the same stream.
#[derive(Clone)]
pub struct SseSender {
    shared: Arc<Shared>,
}

impl SseSender {
    /// Sends one event. Returns false once the stream is closed.
    pub fn send(&self, event: &str, data: &serde_json::Value) -> bool {
        self.send_serialized(event, &serde_json::to_string(data).unwrap_or_else(|_| "null".into()))
    }

    /// Sends one event whose data is already JSON on one line.
    pub fn send_serialized(&self, event: &str, data: &str) -> bool {
        self.enqueue(format!("event: {event}\ndata: {data}\n\n"))
    }

    fn enqueue(&self, text: String) -> bool {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue.closed {
            return false;
        }
        if queue.queued + text.len() > self.shared.limit {
            drop(queue);
            self.close();
            return false;
        }
        queue.queued += text.len();
        queue.chunks.push_back(Bytes::from(text));
        if let Some(waker) = queue.waker.take() {
            waker.wake();
        }
        true
    }

    /// Ends the stream after what is already queued and runs the cleanup.
    pub fn close(&self) {
        let cleanup = {
            let mut queue = self.shared.queue.lock().unwrap();
            if queue.closed {
                return;
            }
            queue.closed = true;
            if let Some(waker) = queue.waker.take() {
                waker.wake();
            }
            queue.cleanup.take()
        };
        self.shared.closed.notify_waiters();
        if let Some(cleanup) = cleanup {
            cleanup();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.shared.queue.lock().unwrap().closed
    }

    /// Resolves once the stream is closed.
    pub async fn closed(&self) {
        loop {
            let notified = self.shared.closed.notified();
            if self.is_closed() {
                return;
            }
            notified.await;
        }
    }
}

/// The response body. Dropping it closes the stream.
struct SseBody {
    sender: SseSender,
}

impl tokio_stream::Stream for SseBody {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut queue = self.sender.shared.queue.lock().unwrap();
        if let Some(chunk) = queue.chunks.pop_front() {
            queue.queued -= chunk.len();
            return Poll::Ready(Some(Ok(chunk)));
        }
        if queue.closed {
            return Poll::Ready(None);
        }
        queue.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for SseBody {
    fn drop(&mut self) {
        self.sender.close();
    }
}

/// Builds an event-stream response. `start` receives the sender and returns
/// the cleanup to run when the stream closes for any reason.
pub fn event_stream(start: impl FnOnce(SseSender) -> Cleanup) -> Response {
    event_stream_with_limit(QUEUE_LIMIT, start)
}

pub fn event_stream_with_limit(limit: usize, start: impl FnOnce(SseSender) -> Cleanup) -> Response {
    let sender = SseSender {
        shared: Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            closed: Notify::new(),
            limit,
        }),
    };
    sender.enqueue("retry: 1500\n\n".into());
    let heartbeat = sender.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(HEARTBEAT) => {
                    if !heartbeat.enqueue(": ping\n\n".into()) {
                        return;
                    }
                }
                _ = heartbeat.closed() => return,
            }
        }
    });
    let cleanup = start(sender.clone());
    {
        let mut queue = sender.shared.queue.lock().unwrap();
        if !queue.closed {
            queue.cleanup = Some(cleanup);
        } else {
            // A synchronous producer can close before returning its cleanup.
            drop(queue);
            cleanup();
        }
    }
    let mut response = Response::new(Body::from_stream(SseBody { sender }));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}
