//! The providers: who serves a model, each through one or more accounts (a
//! subscription login or an API key). Every provider speaks the Oak format
//! (`oak`) to the rest of Oak and its own wire format to its service.

use serde::{Deserialize, Serialize};

/// A provider. Duck.ai is not one: it has its own page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    #[serde(alias = "Claude")]
    Claude,
    #[serde(alias = "OpenRouter")]
    OpenRouter,
    /// A ChatGPT plan's Codex models, through its login.
    #[serde(alias = "Codex")]
    Codex,
    /// Freebuff's models through its own login.
    #[serde(alias = "Freebuff")]
    Freebuff,
    /// The model on the user's own llama.cpp server.
    #[serde(alias = "Local")]
    Local,
    /// NodeClub's models (OpenAI-compatible), through its key.
    #[serde(alias = "NodeClub")]
    NodeClub,
}

/// What a provider's box measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metered {
    /// 5-hour and weekly limits (a subscription).
    Limits,
    /// Credits bought: what is left and what was spent.
    Credits,
    /// Nothing to measure.
    Free,
}

impl Kind {
    /// Every provider, in the order pickers and Home list them.
    pub const ALL: [Kind; 6] = [Kind::Claude, Kind::Codex, Kind::OpenRouter, Kind::NodeClub, Kind::Freebuff, Kind::Local];

    /// Signed in to it: a login or key (for Claude, an account here or a
    /// vault, which has them).
    pub fn on(self) -> bool {
        match self {
            Kind::Claude => !crate::api::accounts().is_empty() || crate::api::vault_on(),
            Kind::OpenRouter => crate::openrouter::on(),
            Kind::Codex => crate::codex::on(),
            Kind::Freebuff => crate::freebuff::on(),
            Kind::Local => crate::local::on(),
            Kind::NodeClub => crate::nodeclub::on(),
        }
    }

    /// Its id in stored data ("claude", "openrouter", …).
    pub fn id(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::OpenRouter => "openrouter",
            Kind::Codex => "codex",
            Kind::Freebuff => "freebuff",
            Kind::Local => "local",
            Kind::NodeClub => "nodeclub",
        }
    }

    /// As Oak shows it.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Claude => "Claude",
            Kind::OpenRouter => "OpenRouter",
            Kind::Codex => "Codex",
            Kind::Freebuff => "Freebuff",
            Kind::Local => "Local",
            Kind::NodeClub => "NodeClub",
        }
    }

    pub fn metered(self) -> Metered {
        match self {
            Kind::Claude | Kind::Codex => Metered::Limits,
            Kind::OpenRouter => Metered::Credits,
            Kind::NodeClub | Kind::Freebuff | Kind::Local => Metered::Free,
        }
    }

    /// Who serves a model named by its id alone (a model a subagent was
    /// asked for, or one from before conversations had a provider):
    /// OpenRouter first, as it also serves the session-title models, which
    /// no other provider lists; Claude when none claims it.
    pub fn of(model: &str) -> Kind {
        if crate::openrouter::routed(model) {
            Kind::OpenRouter
        } else if crate::codex::serves(model) {
            Kind::Codex
        } else if crate::freebuff::serves(model) {
            Kind::Freebuff
        } else if crate::local::serves(model) {
            Kind::Local
        } else if crate::nodeclub::serves(model) {
            Kind::NodeClub
        } else {
            Kind::Claude
        }
    }
}
