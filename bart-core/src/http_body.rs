use reqwest::Response;
use tokio::time::{Instant, timeout_at};

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Complete,
    Excerpt,
}

pub(crate) enum Body {
    Complete(Vec<u8>),
    Excerpt { bytes: Vec<u8>, reason: Error },
}

#[derive(Debug)]
pub(crate) enum Error {
    Transport(reqwest::Error),
    Deadline,
    Overflow,
}

impl Body {
    pub(crate) fn complete(self) -> Result<Vec<u8>, Error> {
        match self {
            Self::Complete(bytes) => Ok(bytes),
            Self::Excerpt { reason, .. } => Err(reason),
        }
    }
}

pub(crate) async fn read(
    mut response: Response,
    limit: usize,
    mode: Mode,
    deadline: Instant,
) -> Body {
    let mut bytes = Vec::new();
    if matches!(mode, Mode::Complete)
        && response
            .content_length()
            .is_some_and(|size| size > limit as u64)
    {
        return Body::Excerpt {
            bytes,
            reason: Error::Overflow,
        };
    }
    loop {
        let next = match timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let room = limit - bytes.len();
                if matches!(mode, Mode::Complete) && chunk.len() > room {
                    return Body::Excerpt {
                        bytes,
                        reason: Error::Overflow,
                    };
                }
                let kept = chunk.len().min(room);
                bytes.extend_from_slice(&chunk[..kept]);
                if kept == chunk.len() {
                    continue;
                }
                Error::Overflow
            }
            Ok(Ok(None)) => return Body::Complete(bytes),
            Ok(Err(error)) => Error::Transport(error),
            Err(_) => Error::Deadline,
        };
        return Body::Excerpt {
            bytes,
            reason: next,
        };
    }
}
