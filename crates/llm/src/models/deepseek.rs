//! `DeepSeek` model ids.

use super::{Reasoning, Thinking};
use crate::Effort;

/// Thinking turns off, and takes `low`, `high` and `max`.
pub const REASONING: Reasoning =
    Reasoning::Takes(Thinking::levels(&[Effort::Low, Effort::High, Effort::Max]).disabling());
