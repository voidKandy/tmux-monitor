use std::time::Duration;

use crate::environment::dispatch::{EnvMessageSender, EnvRequest};
use crate::environment::{
    agent::memory::{messages::MessageRole, Message},
    errors::GptError,
};
use crate::errors::error_chain_fmt;
use anyhow::anyhow;
use futures::Stream;
use futures_util::StreamExt;
use reqwest_streams::error::StreamBodyError;
use serde::Deserialize;

pub type CompletionStream =
    Box<dyn Stream<Item = Result<StreamResponse, StreamBodyError>> + Send + Unpin>;

#[derive(Debug, Deserialize, Clone)]
pub struct StreamResponse {
    pub choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StreamChoice {
    pub delta: StreamDelta,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StreamDelta {
    pub role: Option<String>,
    pub content: Option<String>,
}

#[derive(thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Undefined(#[from] anyhow::Error),
    GptError(#[from] GptError),
    RetryError,
}

impl std::fmt::Debug for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        error_chain_fmt(self, f)
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub type CompletionStreamReceiver =
    tokio::sync::mpsc::Receiver<Result<CompletionStreamStatus, StreamError>>;
pub type CompletionStreamSender =
    tokio::sync::mpsc::Sender<Result<CompletionStreamStatus, StreamError>>;

pub struct StreamedCompletionHandler {
    stream: Option<CompletionStream>,
    sender: Option<CompletionStreamSender>,
    receiver: CompletionStreamReceiver,
    message_content: String,
}

impl std::fmt::Debug for StreamedCompletionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamedCompletionHandler")
            .field("stream", &"<<skipped>>")
            .field("sender", &self.sender)
            .field("receiver", &self.receiver)
            .finish()
    }
}

#[derive(Debug)]
pub enum CompletionStreamStatus {
    Working(String),
    Finished,
}

impl
    From<(
        CompletionStream,
        CompletionStreamSender,
        CompletionStreamReceiver,
    )> for StreamedCompletionHandler
{
    fn from(
        (stream, sender, receiver): (
            CompletionStream,
            CompletionStreamSender,
            CompletionStreamReceiver,
        ),
    ) -> Self {
        Self {
            stream: Some(stream),
            sender: Some(sender),
            receiver,
            message_content: String::new(),
        }
    }
}

#[derive(Debug)]
pub struct CompletionStreamingThread;

impl StreamedCompletionHandler {
    /// Returns tokens until finished, when finished, sends an update cache request with the full
    /// message. Best used in a while loop
    #[tracing::instrument("Receive tokens from completion stream", skip(self, sender))]
    pub async fn receive(
        &mut self,
        agent_id: &str,
        sender: EnvMessageSender,
    ) -> Option<CompletionStreamStatus> {
        if self.sender.is_some() && self.stream.is_some() {
            self.spawn().ok()?;
        }
        tracing::info!("Told thread to run");
        if let Some(result) =
            tokio::time::timeout(Duration::from_millis(1000), self.receiver.recv())
                .await
                .map_err(|_| StreamError::Undefined(anyhow!("Receiver got nothing after 1000ms")))
                .ok()?
        {
            match result.ok()? {
                CompletionStreamStatus::Working(token) => {
                    self.message_content.push_str(&token);
                    return Some(CompletionStreamStatus::Working(token.to_string()));
                }
                CompletionStreamStatus::Finished => {
                    tracing::info!("Stream finished with content: {}", self.message_content);
                    let message = Message::new_assistant(&self.message_content);
                    sender
                        .lock()
                        .await
                        .send(
                            EnvRequest::PushToCache {
                                agent_id: agent_id.to_string(),
                                message,
                            }
                            .into(),
                        )
                        .await
                        .map_err(|_| {
                            StreamError::Undefined(anyhow!("Couldn't send update cache request"))
                        })
                        .ok()?;
                    return Some(CompletionStreamStatus::Finished);
                }
            }
        }
        None
    }

    #[tracing::instrument("Spawn completion stream thread", skip(self))]
    fn spawn(&mut self) -> Result<(), StreamError> {
        let mut stream = self.stream.take().unwrap();
        let tx = self.sender.take().unwrap();
        tracing::info!("Completion thread took stream and sender");
        let _: tokio::task::JoinHandle<Result<(), StreamError>> = tokio::spawn(async move {
            tracing::info!("Thread should run");
            loop {
                tracing::info!("Beginning of completion stream thread loop");
                match CompletionStreamingThread::poll_stream_for_tokens(&mut stream).await {
                    Ok(token_option) => {
                        let status = match token_option {
                            Some(ref token) => CompletionStreamStatus::Working(token.to_string()),
                            None => CompletionStreamStatus::Finished,
                        };
                        tracing::info!("Got status: {:?}", status);

                        let break_loop = match &status {
                            &CompletionStreamStatus::Finished => true,
                            _ => false,
                        };
                        tx.send(Ok(status))
                            .await
                            .map_err(|err| StreamError::Undefined(anyhow!("{:?}", err)))?;

                        if break_loop {
                            break;
                        }
                    }
                    Err(err) => {
                        let error = match err {
                            GptError::Recoverable => StreamError::RetryError,
                            _ => err.into(),
                        };

                        if let Err(_) = tx.send(Err(error)).await {
                            break;
                        }
                    }
                };
            }
            Ok(())
        });

        Ok(())
    }
}

impl CompletionStreamingThread {
    #[tracing::instrument(name = "Get token from stream" skip(stream))]
    async fn poll_stream_for_tokens(
        stream: &mut CompletionStream,
    ) -> Result<Option<String>, GptError> {
        while let Some(Ok(stream_response)) = stream.next().await {
            let parsed_response = stream_response.parse();
            return Ok(parsed_response);
        }

        Ok(None)
    }
}
