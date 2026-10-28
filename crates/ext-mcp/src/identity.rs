//! Private exact launch identity; neither secrets nor their hashes become public IDs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::ServerName;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Identity {
    pub declaration: u64,
    pub server: ServerName,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}
