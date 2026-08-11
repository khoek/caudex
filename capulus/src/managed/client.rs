use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::protocol::{
    ManagementError, ManagementRequest, ManagementResponse, PROTOCOL_MAJOR, RequestEnvelope,
    RequestId, ResponseBody, ResponseEnvelope, decode, encode,
};

#[derive(Clone, Debug)]
pub struct ManagementClientOptions {
    pub socket_path: PathBuf,
    pub timeout: Duration,
}

impl ManagementClientOptions {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ManagementClient {
    options: ManagementClientOptions,
}

impl ManagementClient {
    pub fn new(options: ManagementClientOptions) -> Self {
        Self { options }
    }

    pub fn socket_path(&self) -> &Path {
        &self.options.socket_path
    }

    pub fn request(
        &self,
        request: ManagementRequest,
    ) -> Result<ManagementResponse, ManagementError> {
        let request_id = RequestId::random();
        let payload = encode(&RequestEnvelope {
            request_id,
            minimum_protocol_major: PROTOCOL_MAJOR,
            maximum_protocol_major: PROTOCOL_MAJOR,
            request,
        })?;
        let mut stream = UnixStream::connect(&self.options.socket_path)?;
        stream.set_read_timeout(Some(self.options.timeout))?;
        stream.set_write_timeout(Some(self.options.timeout))?;
        write_frame(&mut stream, &payload)?;
        let response: ResponseEnvelope = decode(&read_frame(&mut stream)?)?;
        if response.request_id != request_id {
            return Err(ManagementError::MismatchedRequestId);
        }
        if response.protocol_major != PROTOCOL_MAJOR {
            return Err(ManagementError::UnsupportedProtocol(
                response.protocol_major,
            ));
        }
        match response.body {
            ResponseBody::Ok(response) => Ok(response),
            ResponseBody::Error(error) => Err(ManagementError::Remote {
                code: error.code,
                message: error.message,
            }),
        }
    }
}

fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<(), ManagementError> {
    let length = u32::try_from(payload.len()).map_err(|_| ManagementError::FrameTooLarge)?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, ManagementError> {
    let mut header = [0_u8; 4];
    read_exact(stream, &mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > super::protocol::MAX_FRAME_BYTES {
        return Err(ManagementError::FrameTooLarge);
    }
    let mut payload = vec![0_u8; length];
    read_exact(stream, &mut payload)?;
    Ok(payload)
}

fn read_exact(stream: &mut UnixStream, buffer: &mut [u8]) -> Result<(), ManagementError> {
    match stream.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(ManagementError::EarlyEof)
        }
        Err(error) => Err(error.into()),
    }
}
