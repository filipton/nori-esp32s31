//! What the wifi setup's `data` says: the name nori's clients list the speaker as.

use alloc::string::String;
use serde::Deserialize;

/// The setup's data field, as it is to be filled in.
pub const EXAMPLE: &str = r#"{"name":"NoriHW"}"#;

const DEFAULT_NAME: &str = "NoriHW";

#[derive(Deserialize)]
struct Setup {
    name: String,
}

/// The name from the setup's `data`, else [`DEFAULT_NAME`].
pub fn name(data: Option<&str>) -> &'static str {
    let name = data.and_then(|d| serde_json::from_str::<Setup>(d).ok()).map(|s| s.name).filter(|n| !n.is_empty());
    if name.is_none() {
        log::info!("no name in the setup's data ({EXAMPLE}); using \"{DEFAULT_NAME}\"");
    }
    name.map_or(DEFAULT_NAME, |n| n.leak())
}
