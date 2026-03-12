//! Simple in-memory IP-based rate limiter middleware for axum.
//!
//! Uses a sliding window algorithm: each IP gets a bucket that tracks request
//! timestamps. Requests older than the window are pruned on access. When the
//! bucket is full, the middleware returns 429 Too Many Requests with a
//! Retry-After header.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Rate limiter configuration and state.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
    max_requests: u32,
    window: Duration,
}

struct RateLimiterInner {
    buckets: HashMap<IpAddr, Vec<Instant>>,
    last_cleanup: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// * `max_requests` — maximum requests allowed per window per IP.
    /// * `window` — sliding window duration.
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                buckets: HashMap::new(),
                last_cleanup: Instant::now(),
            })),
            max_requests,
            window,
        }
    }

    /// Check if a request from `ip` is allowed. Returns the number of
    /// remaining requests in the current window if allowed, or `None` if
    /// the limit is exceeded.
    async fn check(&self, ip: IpAddr) -> Result<u32, Duration> {
        let now = Instant::now();
        let mut inner = self.inner.lock().await;

        // Periodic cleanup of stale buckets (every 5 minutes)
        if now.duration_since(inner.last_cleanup) > Duration::from_secs(300) {
            inner
                .buckets
                .retain(|_, timestamps| timestamps.last().map_or(false, |t| now.duration_since(*t) < self.window));
            inner.last_cleanup = now;
        }

        let timestamps = inner.buckets.entry(ip).or_default();

        // Prune expired entries
        let cutoff = now - self.window;
        timestamps.retain(|t| *t > cutoff);

        if timestamps.len() as u32 >= self.max_requests {
            // Calculate retry-after from oldest entry in window
            let oldest = timestamps.first().unwrap();
            let retry_after = self.window - now.duration_since(*oldest);
            Err(retry_after)
        } else {
            timestamps.push(now);
            let remaining = self.max_requests - timestamps.len() as u32;
            Ok(remaining)
        }
    }
}

/// Extract client IP from the request.
///
/// Priority: X-Forwarded-For (first entry) > X-Real-IP > ConnectInfo.
fn extract_ip(req: &Request) -> Option<IpAddr> {
    // X-Forwarded-For: client, proxy1, proxy2
    if let Some(forwarded) = req.headers().get("x-forwarded-for") {
        if let Ok(val) = forwarded.to_str() {
            if let Some(first) = val.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }

    // X-Real-IP
    if let Some(real_ip) = req.headers().get("x-real-ip") {
        if let Ok(val) = real_ip.to_str() {
            if let Ok(ip) = val.trim().parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }

    // Fall back to connected peer address
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
}

/// Axum middleware function for rate limiting.
pub async fn rate_limit_middleware(
    axum::extract::State(limiter): axum::extract::State<RateLimiter>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let ip = match extract_ip(&req) {
        Some(ip) => ip,
        None => {
            // Can't determine IP — allow the request but log it
            tracing::warn!("rate limiter: could not determine client IP");
            return next.run(req).await;
        }
    };

    match limiter.check(ip).await {
        Ok(remaining) => {
            let mut response = next.run(req).await;
            // Add rate limit headers
            let headers = response.headers_mut();
            headers.insert(
                "X-RateLimit-Limit",
                HeaderValue::from_str(&limiter.max_requests.to_string()).unwrap(),
            );
            headers.insert(
                "X-RateLimit-Remaining",
                HeaderValue::from_str(&remaining.to_string()).unwrap(),
            );
            response
        }
        Err(retry_after) => {
            let secs = retry_after.as_secs() + 1; // Round up
            tracing::debug!(%ip, "rate limit exceeded, retry after {secs}s");
            let mut response = (
                StatusCode::TOO_MANY_REQUESTS,
                format!("Rate limit exceeded. Try again in {secs} seconds."),
            )
                .into_response();
            let headers = response.headers_mut();
            headers.insert(
                "Retry-After",
                HeaderValue::from_str(&secs.to_string()).unwrap(),
            );
            headers.insert(
                "X-RateLimit-Limit",
                HeaderValue::from_str(&limiter.max_requests.to_string()).unwrap(),
            );
            headers.insert(
                "X-RateLimit-Remaining",
                HeaderValue::from(0),
            );
            response
        }
    }
}
