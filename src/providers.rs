//! The providers: who serves a model, each through one or more accounts (a
//! subscription login or an API key). Every provider speaks the Oak format
//! (`oak`) to the rest of Oak and its own wire format to its service.

use serde::{Deserialize, Serialize};

/// A provider.
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
    /// Refuseless's abliterated models (OpenAI-compatible), through its
    /// key, paid from its credits.
    #[serde(alias = "Refuseless")]
    Refuseless,
    /// Meta's Muse models (Muse Code's), through a Meta Model API key,
    /// paid per token.
    #[serde(alias = "MuseCode")]
    MuseCode,
    /// Amazon Quick's own agent (`crate::quick`): it runs its own system
    /// prompt, agent loop and model, and Oak runs the tools it asks for.
    #[serde(alias = "Quick")]
    Quick,
    /// Models a plugin serves (`crate::provided`): named for the plugin's
    /// provider ("Duck.ai") wherever providers are shown.
    #[serde(alias = "Plugin")]
    Plugin,
}

/// What a provider's box measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metered {
    /// A subscription's limits: Claude's and Codex's 5-hour and weekly
    /// windows, Quick's month.
    Limits,
    /// Credits bought: what is left and what was spent.
    Credits,
    /// Billed afterwards: what was spent through Oak.
    Spent,
    /// Nothing to measure.
    Free,
}

impl Kind {
    /// Every provider, in the order pickers and Home list them.
    pub const ALL: [Kind; 10] = [Kind::Claude, Kind::Codex, Kind::OpenRouter, Kind::MuseCode, Kind::NodeClub, Kind::Freebuff, Kind::Refuseless, Kind::Quick, Kind::Local, Kind::Plugin];

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
            Kind::Refuseless => crate::refuseless::on(),
            Kind::MuseCode => crate::musecode::on(),
            Kind::Quick => crate::quick::on(),
            Kind::Plugin => crate::provided::on(),
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
            Kind::Refuseless => "refuseless",
            Kind::MuseCode => "musecode",
            Kind::Quick => "quick",
            Kind::Plugin => "plugin",
        }
    }

    /// `id`'s provider, as stored data has it. None: no provider has it.
    pub fn from_id(id: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| k.id() == id)
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
            Kind::Refuseless => "Refuseless",
            Kind::MuseCode => "Muse Code",
            Kind::Quick => "AWS Quick",
            Kind::Plugin => crate::provided::name(),
        }
    }

    pub fn metered(self) -> Metered {
        match self {
            Kind::Claude | Kind::Codex | Kind::Quick => Metered::Limits,
            Kind::OpenRouter | Kind::Refuseless => Metered::Credits,
            Kind::MuseCode => Metered::Spent,
            Kind::NodeClub | Kind::Freebuff | Kind::Local | Kind::Plugin => Metered::Free,
        }
    }

    /// Who serves a model named by its id alone (a model a subagent was
    /// asked for, or one from before conversations had a provider):
    /// OpenRouter first, as it also serves the session-title models, which
    /// no other provider lists; Claude when none claims it.
    pub fn of(model: &str) -> Kind {
        if crate::provided::serves(model) {
            Kind::Plugin
        } else if crate::openrouter::routed(model) {
            Kind::OpenRouter
        } else if crate::codex::serves(model) {
            Kind::Codex
        } else if crate::freebuff::serves(model) {
            Kind::Freebuff
        } else if crate::local::serves(model) {
            Kind::Local
        } else if crate::nodeclub::serves(model) {
            Kind::NodeClub
        } else if crate::refuseless::serves(model) {
            Kind::Refuseless
        } else if crate::musecode::serves(model) {
            Kind::MuseCode
        } else if crate::quick::serves(model) {
            Kind::Quick
        } else {
            Kind::Claude
        }
    }
}
