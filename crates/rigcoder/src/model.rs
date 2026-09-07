//! The model as a handler entity: a provider `CompletionModel` wrapped in
//! rig's `CompletionAdapter`, registered under one key.

use bevy_ecs::prelude::*;
use rig::{
    client::CompletionClient,
    error::ErrorReport,
    prelude::*,
    providers::{anthropic, gemini, openai},
    serve::adapters::CompletionAdapter,
};
use rig_ecs::bus::Handlers;

/// The key the agent's `UsesModel` handler entity is registered under.
pub const MODEL_KEY: &str = "rigcoder/model";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Gemini,
}

/// Which provider and model to register. Read from `RIGCODER_PROVIDER`
/// (`anthropic` | `openai` | `gemini`) and `RIGCODER_MODEL`; the API key comes
/// from the provider's usual variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
/// `GEMINI_API_KEY`).
#[derive(Resource, Debug, Clone)]
pub struct ModelChoice {
    pub provider: Provider,
    pub model: String,
}

impl ModelChoice {
    pub const DEFAULT_ANTHROPIC: &str = "claude-opus-5";
    pub const DEFAULT_OPENAI: &str = "gpt-5.6-sol";
    pub const DEFAULT_GEMINI: &str = "gemini-3.8-flash";

    pub fn from_env() -> Self {
        let provider = match std::env::var("RIGCODER_PROVIDER").as_deref() {
            Ok("openai") => Provider::OpenAi,
            Ok("gemini") => Provider::Gemini,
            _ => Provider::Anthropic,
        };
        let model = std::env::var("RIGCODER_MODEL").unwrap_or_else(|_| match provider {
            Provider::Anthropic => Self::DEFAULT_ANTHROPIC.to_owned(),
            Provider::OpenAi => Self::DEFAULT_OPENAI.to_owned(),
            Provider::Gemini => Self::DEFAULT_GEMINI.to_owned(),
        });
        Self { provider, model }
    }

    pub fn parse(provider: &str, model: Option<String>) -> anyhow::Result<Self> {
        let provider = match provider {
            "anthropic" => Provider::Anthropic,
            "openai" => Provider::OpenAi,
            "gemini" => Provider::Gemini,
            other => anyhow::bail!("unknown provider {other:?}; expected anthropic, openai or gemini"),
        };
        let model = model.unwrap_or_else(|| match provider {
            Provider::Anthropic => Self::DEFAULT_ANTHROPIC.to_owned(),
            Provider::OpenAi => Self::DEFAULT_OPENAI.to_owned(),
            Provider::Gemini => Self::DEFAULT_GEMINI.to_owned(),
        });
        Ok(Self { provider, model })
    }
}

impl std::fmt::Display for ModelChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let provider = match self.provider {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
            Provider::Gemini => "gemini",
        };
        write!(f, "{provider}/{}", self.model)
    }
}

/// Register the chosen model under [`MODEL_KEY`]; the handler entity is what
/// the agent's `UsesModel` points at.
pub fn register(handlers: &mut Handlers, choice: &ModelChoice) -> Result<Entity, ErrorReport> {
    let label = choice.model.as_str();
    match choice.provider {
        Provider::Anthropic => {
            let client = anthropic::Client::from_env().map_err(provider_error)?;
            let model = client.completion_model(label);
            handlers.register(MODEL_KEY, CompletionAdapter::new(label, model))
        }
        Provider::OpenAi => {
            let client = openai::Client::from_env().map_err(provider_error)?;
            let model = client.completion_model(label);
            handlers.register(MODEL_KEY, CompletionAdapter::new(label, model))
        }
        Provider::Gemini => {
            let client = gemini::Client::from_env().map_err(provider_error)?;
            let model = client.completion_model(label);
            handlers.register(MODEL_KEY, CompletionAdapter::new(label, model))
        }
    }
}

fn provider_error(error: impl std::fmt::Display) -> ErrorReport {
    ErrorReport::new(rig::error::ErrorKind::HandlerUnavailable, error.to_string())
}
