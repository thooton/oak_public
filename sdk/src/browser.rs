//! Oak's background Chromium session for a Home plugin's own form.
//! Observing and sending input use Chromium's DevTools protocol, not scripts.

use crate::host::checked;
use crate::rt::{api, s};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Browser(String);

impl Browser {
    pub(crate) fn new(handle: String) -> Self {
        Self(handle)
    }

    pub fn same_session(&self, other: &Self) -> bool {
        self.0 == other.0
    }

    fn command(&self, command: Command) -> Result<String, String> {
        let request = serde_json::to_string(&command).map_err(|_| "Invalid browser command.")?;
        checked(|out, user| unsafe {
            (api().background_browser_command)(s(&self.0), s(&request), out, user)
        })
    }

    /// Visible accessibility labels and controls; never returns field values.
    pub fn observe(&self) -> Result<Screen, String> {
        serde_json::from_str(&self.command(Command::Observe)?)
            .map_err(|_| "Invalid browser response.".into())
    }

    /// Focuses the field, replaces its text, and sends native input events.
    /// A changed document or a missing target fails without sending text.
    pub fn fill(&self, screen: &Screen, node: u64, text: &str) -> Result<(), String> {
        self.command(Command::Fill {
            screen: screen.address.clone(),
            node,
            text: text.to_owned(),
        })
        .map(|_| ())
    }

    /// Sends one native click to a button from this screen.
    pub fn click(&self, screen: &Screen, node: u64) -> Result<(), String> {
        self.command(Command::Click {
            screen: screen.address.clone(),
            node,
        })
        .map(|_| ())
    }

    /// Closes the browser and discards its private profile.
    pub fn close(&self) {
        let _ = self.command(Command::Close);
    }
}

#[doc(hidden)]
#[derive(Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Command {
    Observe,
    Fill {
        screen: Address,
        node: u64,
        text: String,
    },
    Click {
        screen: Address,
        node: u64,
    },
    Close,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Address {
    pub origin: String,
    pub path: String,
    #[doc(hidden)]
    pub document: String,
}

#[derive(Deserialize, Serialize)]
pub struct Screen {
    pub address: Address,
    pub nodes: Vec<Node>,
}

#[derive(Deserialize, Serialize)]
pub struct Node {
    pub id: u64,
    pub role: String,
    pub label: String,
    pub disabled: bool,
    pub invalid: bool,
    pub busy: bool,
    /// Metadata from the element, excluding its text and value attributes.
    pub input_type: Option<String>,
    pub autocomplete: Option<String>,
    pub test_id: Option<String>,
}
