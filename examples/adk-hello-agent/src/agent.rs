//! The agent's "brain": turn a subject into a one-line greeting.
//!
//! `LlmBrain` runs a real adk-rust `LlmAgent` backed by Anthropic Claude.
//! `StaticBrain` returns a fixed line so integration tests — and headless
//! demos — need no API key and stay deterministic. `main` picks `LlmBrain`
//! when `ANTHROPIC_API_KEY` is set, else `StaticBrain`.
//!
//! Note what is NOT here: adk-rust's `Launcher`/`adk-server`. We use only
//! the library crates (agents/models/runner/sessions); Triton is the front
//! door, so adk-rust's own A2A/REST server is the thing we replace.

use std::sync::Arc;

use anyhow::Result;

/// Turn a subject into a one-line greeting. The only seam the HTTP layer
/// depends on, so tests can swap a deterministic brain for the live LLM.
#[async_trait::async_trait]
pub trait Brain: Send + Sync {
    async fn greet(&self, subject: &str) -> Result<String>;
}

/// Deterministic brain for tests and no-key local/CI runs.
pub struct StaticBrain;

#[async_trait::async_trait]
impl Brain for StaticBrain {
    async fn greet(&self, subject: &str) -> Result<String> {
        Ok(format!("Hello, {subject}! Welcome to Triton."))
    }
}

/// Live brain: a real adk-rust `LlmAgent` backed by Anthropic Claude,
/// driven through the programmatic `Runner` (no interactive launcher).
pub struct LlmBrain {
    runner: adk_rust::runner::Runner,
    session_service: Arc<adk_rust::session::InMemorySessionService>,
}

impl LlmBrain {
    /// Build the agent once. Reads `ANTHROPIC_API_KEY` from the env.
    pub fn from_env() -> Result<Self> {
        use adk_rust::prelude::*;

        let api_key = std::env::var("ANTHROPIC_API_KEY")?;
        let model = AnthropicClient::new(AnthropicConfig::new(&api_key, "claude-sonnet-4-6"))?;

        let agent = LlmAgentBuilder::new("hello_agent")
            .description("A friendly greeter for the Triton hello-world demo")
            .instruction(
                "Greet the given subject warmly in exactly one short, friendly \
                 sentence. Output only the greeting — no preamble, no quotes.",
            )
            .model(Arc::new(model))
            .build()?;

        let session_service = Arc::new(adk_rust::session::InMemorySessionService::new());
        // `RunnerConfig` is `#[non_exhaustive]`; the builder is the
        // supported construction path.
        let runner = adk_rust::runner::Runner::builder()
            .app_name("hello")
            .agent(Arc::new(agent))
            .session_service(session_service.clone())
            .build()?;

        Ok(Self {
            runner,
            session_service,
        })
    }
}

#[async_trait::async_trait]
impl Brain for LlmBrain {
    async fn greet(&self, subject: &str) -> Result<String> {
        use adk_rust::futures::StreamExt;
        use adk_rust::prelude::*;
        use adk_rust::session::{CreateRequest, SessionService};
        use adk_rust::{SessionId, UserId};

        // Each greeting is a fresh single-turn session — the agent is
        // stateless across calls, matching Triton's per-call contract.
        let session = self
            .session_service
            .create(CreateRequest {
                app_name: "hello".to_string(),
                user_id: "triton".to_string(),
                session_id: None,
                state: Default::default(),
            })
            .await?;

        let prompt = Content::new("user").with_text(format!("Subject: {subject}"));
        let mut stream = self
            .runner
            .run(
                UserId::new("triton")?,
                SessionId::new(session.id())?,
                prompt,
            )
            .await?;

        let mut out = String::new();
        while let Some(event) = stream.next().await {
            let event = event?;
            if let Some(content) = event.content() {
                for part in &content.parts {
                    if let Part::Text { text } = part {
                        out.push_str(text);
                    }
                }
            }
        }
        Ok(out.trim().to_string())
    }
}
