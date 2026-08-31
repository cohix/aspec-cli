//! `UserMessage` and `UserMessageSink` — Layer 0.
//!
//! Plain message types and sink traits with no dependencies, importable by
//! every layer (notably `data::issue`, which reports GitHub fetch progress).
//! All engines write status messages to the user through a `UserMessageSink`.
//! Layer 3 implements one sink per concrete frontend type. The CLI sink queues
//! while a PTY-bound container owns the terminal and replays after the
//! container releases it; TUI and API sinks render live and treat
//! `replay_queued` as a no-op.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub level: MessageLevel,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warning,
    Error,
    Success,
}

/// A sink for awman-authored status messages displayed in the awman UI, NOT
/// inside a container's terminal window. Defined by Layer 1; implemented by
/// Layer 3.
pub trait UserMessageSink: Send + Sync {
    /// Write a message immediately if the output device is available, or queue
    /// it for later replay.
    fn write_message(&mut self, msg: UserMessage);

    /// Drain queued messages (no-op for sinks that render live). Idempotent.
    fn replay_queued(&mut self);

    fn info(&mut self, text: impl Into<String>)
    where
        Self: Sized,
    {
        self.write_message(UserMessage {
            level: MessageLevel::Info,
            text: text.into(),
        });
    }

    fn warning(&mut self, text: impl Into<String>)
    where
        Self: Sized,
    {
        self.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: text.into(),
        });
    }

    fn error_msg(&mut self, text: impl Into<String>)
    where
        Self: Sized,
    {
        self.write_message(UserMessage {
            level: MessageLevel::Error,
            text: text.into(),
        });
    }

    fn success(&mut self, text: impl Into<String>)
    where
        Self: Sized,
    {
        self.write_message(UserMessage {
            level: MessageLevel::Success,
            text: text.into(),
        });
    }
}

/// Test/utility sink that records every message passed to it. Used by engine
/// unit tests in 0067 and by the CLI when it queues during PTY ownership.
#[derive(Debug, Default)]
pub struct RecordingMessageSink {
    queue: Vec<UserMessage>,
    replayed: Vec<UserMessage>,
}

impl RecordingMessageSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently-queued (not-yet-replayed) messages.
    pub fn queued(&self) -> &[UserMessage] {
        &self.queue
    }

    /// Messages that have been drained via `replay_queued`.
    pub fn replayed(&self) -> &[UserMessage] {
        &self.replayed
    }

    /// All messages ever written, in insertion order.
    pub fn all(&self) -> Vec<UserMessage> {
        let mut v = self.replayed.clone();
        v.extend_from_slice(&self.queue);
        v
    }
}

impl UserMessageSink for RecordingMessageSink {
    fn write_message(&mut self, msg: UserMessage) {
        self.queue.push(msg);
    }

    fn replay_queued(&mut self) {
        self.replayed.append(&mut self.queue);
    }
}

/// A sink that writes messages straight to stderr, prefixed like the rest of
/// awman's stderr output. Used where a message must stay visible but no
/// frontend is available to hold it — notably the ACP client's reader task,
/// which surfaces protocol warnings and agent stderr from a detached tokio task
/// (a `RecordingMessageSink` there would buffer into a `Vec` nobody ever reads,
/// silently swallowing every "ignoring malformed line" / "ACP agent stderr"
/// warning in production).
#[derive(Debug, Default)]
pub struct StderrMessageSink;

impl StderrMessageSink {
    pub fn new() -> Self {
        Self
    }
}

impl UserMessageSink for StderrMessageSink {
    fn write_message(&mut self, msg: UserMessage) {
        let prefix = match msg.level {
            MessageLevel::Warning => "awman warning: ",
            MessageLevel::Error => "awman error: ",
            MessageLevel::Info | MessageLevel::Success => "awman: ",
        };
        eprintln!("{prefix}{}", msg.text);
    }

    fn replay_queued(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_message_queues() {
        let mut s = RecordingMessageSink::new();
        s.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "hi".into(),
        });
        assert_eq!(s.queued().len(), 1);
        assert_eq!(s.replayed().len(), 0);
    }

    #[test]
    fn replay_drains_in_order() {
        let mut s = RecordingMessageSink::new();
        s.write_message(UserMessage {
            level: MessageLevel::Info,
            text: "a".into(),
        });
        s.write_message(UserMessage {
            level: MessageLevel::Warning,
            text: "b".into(),
        });
        s.replay_queued();
        assert_eq!(s.queued().len(), 0);
        assert_eq!(s.replayed().len(), 2);
        assert_eq!(s.replayed()[0].text, "a");
        assert_eq!(s.replayed()[1].text, "b");
    }

    #[test]
    fn replay_is_idempotent() {
        let mut s = RecordingMessageSink::new();
        s.replay_queued();
        s.replay_queued();
    }

    #[test]
    fn convenience_info_writes_info_level() {
        let mut s = RecordingMessageSink::new();
        s.info("hi");
        assert_eq!(s.queued().len(), 1);
        assert_eq!(s.queued()[0].level, MessageLevel::Info);
        assert_eq!(s.queued()[0].text, "hi");
    }

    #[test]
    fn convenience_warning_writes_warning_level() {
        let mut s = RecordingMessageSink::new();
        s.warning("w");
        assert_eq!(s.queued().len(), 1);
        assert_eq!(s.queued()[0].level, MessageLevel::Warning);
    }

    #[test]
    fn convenience_error_msg_writes_error_level() {
        let mut s = RecordingMessageSink::new();
        s.error_msg("e");
        assert_eq!(s.queued().len(), 1);
        assert_eq!(s.queued()[0].level, MessageLevel::Error);
    }

    #[test]
    fn convenience_success_writes_success_level() {
        let mut s = RecordingMessageSink::new();
        s.success("ok");
        assert_eq!(s.queued().len(), 1);
        assert_eq!(s.queued()[0].level, MessageLevel::Success);
    }
}
