pub mod claude;
pub mod codex;
pub mod copilot;
pub mod opencode;

use crate::model::{Session, Tool};

/// One conversation turn of a session transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// A session store. Adding a new AI tool means one new implementation here.
pub trait SessionSource {
    fn tool(&self) -> Tool;
    fn available(&self) -> bool;
    /// Session metadata for every session in the store (no transcript bodies).
    fn list(&self) -> Result<Vec<Session>, String>;
    /// Ordered (role, text) turns for one session; text content only.
    fn transcript(&self, id: &str) -> Result<Vec<Turn>, String>;
}

/// All known sources, constructed from the environment.
pub fn sources_from_env() -> Vec<Box<dyn SessionSource>> {
    vec![
        Box::new(claude::ClaudeSource::from_env()),
        Box::new(opencode::OpencodeSource::from_env()),
        Box::new(codex::CodexSource::from_env()),
        Box::new(copilot::CopilotSource::from_env()),
    ]
}
