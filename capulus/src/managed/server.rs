use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::net::sockopt::socket_peercred;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore};

use super::protocol::{
    ErrorCode, ManagementError, ManagementRequest, ManagementResponse, PROTOCOL_MAJOR,
    PeerCredentials, ProtocolError, RequestEnvelope, ResponseBody, ResponseEnvelope, decode,
    encode,
};

pub trait ManagementHandler: Send + Sync + 'static {
    fn handle(
        &self,
        peer: PeerCredentials,
        request: ManagementRequest,
    ) -> impl Future<Output = Result<ManagementResponse, ProtocolError>> + Send;
}

#[derive(Clone, Debug)]
pub struct ManagementServerOptions {
    pub connection_limit: usize,
    pub request_timeout: Duration,
    pub requests_per_minute: u32,
}

impl Default for ManagementServerOptions {
    fn default() -> Self {
        Self {
            connection_limit: 32,
            request_timeout: Duration::from_secs(30),
            requests_per_minute: 120,
        }
    }
}

pub struct ManagementServer<H> {
    listener: UnixListener,
    handler: Arc<H>,
    options: ManagementServerOptions,
}

impl<H: ManagementHandler> ManagementServer<H> {
    pub fn new(
        listener: UnixListener,
        handler: Arc<H>,
        options: ManagementServerOptions,
    ) -> Result<Self, ManagementError> {
        if !(1..=4096).contains(&options.connection_limit) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "management connection limit must be between 1 and 4096",
            )
            .into());
        }
        if !(Duration::from_secs(1)..=Duration::from_secs(5 * 60))
            .contains(&options.request_timeout)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "management request timeout must be between 1 second and 5 minutes",
            )
            .into());
        }
        if !(1..=10_000).contains(&options.requests_per_minute) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "management request rate must be between 1 and 10000 per minute",
            )
            .into());
        }
        Ok(Self {
            listener,
            handler,
            options,
        })
    }

    pub async fn run(self) -> Result<(), ManagementError> {
        let permits = Arc::new(Semaphore::new(self.options.connection_limit));
        let rates = Arc::new(RequestRateLimiter::new(
            self.options.requests_per_minute,
            self.options.connection_limit.saturating_mul(4),
        ));
        loop {
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .expect("management semaphore is never closed");
            let (stream, _) = self.listener.accept().await?;
            let handler = Arc::clone(&self.handler);
            let rates = Arc::clone(&rates);
            let timeout = self.options.request_timeout;
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) =
                    tokio::time::timeout(timeout, serve_connection(stream, handler, rates))
                        .await
                        .map_err(|_| {
                            ManagementError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut))
                        })
                        .and_then(|result| result)
                {
                    eprintln!("capulus management connection failed: {error}");
                }
            });
        }
    }
}

async fn serve_connection<H: ManagementHandler>(
    mut stream: UnixStream,
    handler: Arc<H>,
    rates: Arc<RequestRateLimiter>,
) -> Result<(), ManagementError> {
    let credentials =
        socket_peercred(&stream).map_err(|error| ManagementError::Io(error.into()))?;
    let peer = PeerCredentials {
        pid: credentials.pid.as_raw_nonzero().get() as u32,
        uid: credentials.uid.as_raw(),
        gid: credentials.gid.as_raw(),
    };
    let allowed = rates.allow(peer.uid).await;
    let request: RequestEnvelope = decode(&read_frame(&mut stream).await?)?;
    let response = if !allowed {
        ResponseBody::Error(ProtocolError::new(
            ErrorCode::Unavailable,
            "management request rate limit exceeded",
        ))
    } else if request.minimum_protocol_major <= PROTOCOL_MAJOR
        && request.maximum_protocol_major >= PROTOCOL_MAJOR
    {
        match handler.handle(peer, request.request).await {
            Ok(response) => ResponseBody::Ok(response),
            Err(error) => ResponseBody::Error(error),
        }
    } else {
        ResponseBody::Error(ProtocolError::new(
            ErrorCode::UnsupportedProtocol,
            format!("agent supports management protocol v{PROTOCOL_MAJOR}"),
        ))
    };
    write_frame(
        &mut stream,
        &encode(&ResponseEnvelope {
            request_id: request.request_id,
            protocol_major: PROTOCOL_MAJOR,
            body: response,
        })?,
    )
    .await
}

struct RequestRateLimiter {
    maximum_requests: u32,
    maximum_uids: usize,
    windows: Mutex<HashMap<u32, RequestWindow>>,
}

impl RequestRateLimiter {
    fn new(maximum_requests: u32, maximum_uids: usize) -> Self {
        Self {
            maximum_requests,
            maximum_uids,
            windows: Mutex::new(HashMap::new()),
        }
    }

    async fn allow(&self, uid: u32) -> bool {
        let mut windows = self.windows.lock().await;
        self.allow_at(uid, Instant::now(), &mut windows)
    }

    fn allow_at(&self, uid: u32, now: Instant, windows: &mut HashMap<u32, RequestWindow>) -> bool {
        const WINDOW: Duration = Duration::from_secs(60);

        windows.retain(|_, window| now.duration_since(window.started) < WINDOW);
        if let Some(window) = windows.get_mut(&uid) {
            if window.requests >= self.maximum_requests {
                return false;
            }
            window.requests += 1;
            return true;
        }
        if windows.len() >= self.maximum_uids
            && let Some(oldest) = windows
                .iter()
                .min_by_key(|(_, window)| window.started)
                .map(|(uid, _)| *uid)
        {
            windows.remove(&oldest);
        }
        windows.insert(
            uid,
            RequestWindow {
                started: now,
                requests: 1,
            },
        );
        true
    }
}

#[derive(Clone, Copy)]
struct RequestWindow {
    started: Instant,
    requests: u32,
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, ManagementError> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(ManagementError::EarlyEof);
        }
        Err(error) => return Err(error.into()),
    };
    if length > super::protocol::MAX_FRAME_BYTES {
        return Err(ManagementError::FrameTooLarge);
    }
    let mut payload = vec![0_u8; length];
    match stream.read_exact(&mut payload).await {
        Ok(_) => Ok(payload),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(ManagementError::EarlyEof)
        }
        Err(error) => Err(error.into()),
    }
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<(), ManagementError> {
    stream
        .write_u32(u32::try_from(payload.len()).map_err(|_| ManagementError::FrameTooLarge)?)
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_uid_rate_windows_are_bounded_and_reset() {
        let limiter = RequestRateLimiter::new(2, 2);
        let started = Instant::now();
        let mut windows = HashMap::new();

        assert!(limiter.allow_at(1000, started, &mut windows));
        assert!(limiter.allow_at(1000, started, &mut windows));
        assert!(!limiter.allow_at(1000, started, &mut windows));
        assert!(limiter.allow_at(1001, started, &mut windows));
        assert!(limiter.allow_at(1002, started, &mut windows));
        assert_eq!(windows.len(), 2);
        assert!(limiter.allow_at(1000, started + Duration::from_secs(61), &mut windows));
    }
}
