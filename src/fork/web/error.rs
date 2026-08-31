//! Error categories shared by the WEB relay session, manager, and HTTP layer.

/// Failure classes that the carrier maps onto HTTP responses.
///
/// Every category except `Backpressure` and `Concurrent` is answered with the
/// site's ordinary 404 body so an unauthenticated prober cannot distinguish a
/// relay endpoint from an unknown static path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebError {
    /// The bearer is unknown, expired, or malformed.
    Authentication,
    /// A queue budget is temporarily exhausted; the request may be retried.
    Backpressure,
    /// A capacity or rate limit rejected the request.
    Limit,
    /// The peer violated the wire contract; the session or lane is closed.
    Protocol,
    /// Another request of the same kind is already in flight.
    Concurrent,
    /// The session is closed.
    Closed,
}
