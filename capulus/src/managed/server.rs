use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rustix::net::sockopt::socket_peercred;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

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
}

impl Default for ManagementServerOptions {
    fn default() -> Self {
        Self {
            connection_limit: 32,
            request_timeout: Duration::from_secs(30),
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
        if options.connection_limit == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "management connection limit must be nonzero",
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
        loop {
            let (stream, _) = self.listener.accept().await?;
            let permit = Arc::clone(&permits)
                .acquire_owned()
                .await
                .expect("management semaphore is never closed");
            let handler = Arc::clone(&self.handler);
            let timeout = self.options.request_timeout;
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(error) = tokio::time::timeout(timeout, serve_connection(stream, handler))
                    .await
                    .map_err(|_| {
                        ManagementError::Io(std::io::Error::from(std::io::ErrorKind::TimedOut))
                    })
                    .and_then(|result| result)
                {
                    eprintln!("Capulus management connection failed: {error}");
                }
            });
        }
    }
}

async fn serve_connection<H: ManagementHandler>(
    mut stream: UnixStream,
    handler: Arc<H>,
) -> Result<(), ManagementError> {
    let credentials =
        socket_peercred(&stream).map_err(|error| ManagementError::Io(error.into()))?;
    let peer = PeerCredentials {
        pid: credentials.pid.as_raw_nonzero().get() as u32,
        uid: credentials.uid.as_raw(),
        gid: credentials.gid.as_raw(),
    };
    let request: RequestEnvelope = decode(&read_frame(&mut stream).await?)?;
    let response = if request.minimum_protocol_major <= PROTOCOL_MAJOR
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
