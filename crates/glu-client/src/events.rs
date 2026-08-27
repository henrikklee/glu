use crate::download::DownloadProgress;
use crate::install::dag::{ExecNode, ExecutionPlan};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeCompletionStatus {
    Error,
    Ok,
}

impl NodeCompletionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Ok => "ok",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressFinishStatus {
    Done,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug)]
pub enum ProgressEvent {
    UpgradeDownloadStarted {
        version: String,
    },
    InstallStarted {
        plan: ExecutionPlan,
        install_total: usize,
        dynamic: bool,
        download_progress: DownloadProgress,
    },
    InstallNodeStarted {
        node: ExecNode,
    },
    InstallNodeCompleted {
        node: ExecNode,
        status: NodeCompletionStatus,
    },
    InstallTick,
    InstallFinished {
        status: ProgressFinishStatus,
    },
}

pub trait ExecutionEvents: Send + Sync {
    fn wants_progress(&self) -> bool {
        true
    }

    fn progress(&self, event: ProgressEvent);
    fn notice(&self, stream: OutputStream, message: &str);
}

#[derive(Default)]
pub struct SilentExecutionEvents;

impl ExecutionEvents for SilentExecutionEvents {
    fn wants_progress(&self) -> bool {
        false
    }

    fn progress(&self, _event: ProgressEvent) {}

    fn notice(&self, _stream: OutputStream, _message: &str) {}
}

#[derive(Clone, Debug)]
pub enum RecordedExecutionEvent {
    Progress(Box<ProgressEvent>),
    Notice {
        stream: OutputStream,
        message: String,
    },
}

#[derive(Default)]
pub struct RecordingExecutionEvents {
    events: Mutex<Vec<RecordedExecutionEvent>>,
}

impl RecordingExecutionEvents {
    pub fn events(&self) -> Vec<RecordedExecutionEvent> {
        self.events.lock().expect("events lock poisoned").clone()
    }
}

impl ExecutionEvents for RecordingExecutionEvents {
    fn progress(&self, event: ProgressEvent) {
        self.events
            .lock()
            .expect("events lock poisoned")
            .push(RecordedExecutionEvent::Progress(Box::new(event)));
    }

    fn notice(&self, stream: OutputStream, message: &str) {
        if message.trim().is_empty() {
            return;
        }
        self.events
            .lock()
            .expect("events lock poisoned")
            .push(RecordedExecutionEvent::Notice {
                stream,
                message: message.to_string(),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_sink_preserves_typed_events_and_drops_empty_notices() {
        let events = RecordingExecutionEvents::default();
        events.progress(ProgressEvent::UpgradeDownloadStarted {
            version: "1.2.3".to_string(),
        });
        events.notice(OutputStream::Stderr, "warning");
        events.notice(OutputStream::Stdout, "  ");

        let recorded = events.events();
        assert!(matches!(
            &recorded[0],
            RecordedExecutionEvent::Progress(event)
                if matches!(event.as_ref(), ProgressEvent::UpgradeDownloadStarted { version } if version == "1.2.3")
        ));
        assert!(matches!(
            &recorded[1],
            RecordedExecutionEvent::Notice { stream: OutputStream::Stderr, message }
                if message == "warning"
        ));
        assert_eq!(recorded.len(), 2);
    }
}
