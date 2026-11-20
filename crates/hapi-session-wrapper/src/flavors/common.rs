#[path = "claude/attachments.rs"]
#[cfg(windows)]
mod attachments;
#[cfg(unix)]
pub(crate) use super::claude::attachments::format_message;
#[cfg(windows)]
pub(crate) use attachments::format_message;
