use crate::command_model::GlobalOptions;
use crate::progress::{FinishStatus, ProgressFooter};
use glu_client::events::{
    ExecutionEvents, OutputStream, ProgressEvent, ProgressFinishStatus, SilentExecutionEvents,
};
use glu_client::install::scheduler::ExecutionObserver;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

pub(crate) fn for_invocation(globals: &GlobalOptions) -> Arc<dyn ExecutionEvents> {
    if !globals.is_json() && !globals.is_null() && !globals.plan {
        if std::io::stdout().is_terminal() {
            Arc::new(HumanTtyExecutionEvents::default())
        } else {
            Arc::new(HumanPlainExecutionEvents::default())
        }
    } else {
        Arc::new(SilentExecutionEvents)
    }
}

#[derive(Default)]
struct HumanTtyExecutionEvents {
    footer: Mutex<Option<Arc<ProgressFooter>>>,
}

#[derive(Default)]
struct HumanPlainExecutionEvents {
    footer: Mutex<Option<Arc<ProgressFooter>>>,
}

fn render_progress(
    footer_slot: &Mutex<Option<Arc<ProgressFooter>>>,
    tty: bool,
    event: ProgressEvent,
) {
    match event {
        ProgressEvent::UpgradeDownloadStarted { version } => {
            println!("Downloading glu {version}...");
        }
        ProgressEvent::InstallStarted {
            plan,
            install_total,
            dynamic,
            download_progress,
        } => {
            let footer = Arc::new(ProgressFooter::from_plan(
                &plan,
                install_total,
                tty && dynamic,
                download_progress,
            ));
            footer.start();
            *footer_slot.lock().expect("footer lock poisoned") = Some(footer);
        }
        ProgressEvent::InstallNodeStarted { node } => {
            if let Some(footer) = footer_slot.lock().expect("footer lock poisoned").as_ref() {
                footer.node_started(&node);
            }
        }
        ProgressEvent::InstallNodeCompleted { node, status } => {
            if let Some(footer) = footer_slot.lock().expect("footer lock poisoned").as_ref() {
                footer.node_completed(&node, status);
            }
        }
        ProgressEvent::InstallTick => {
            if let Some(footer) = footer_slot.lock().expect("footer lock poisoned").as_ref() {
                footer.tick();
            }
        }
        ProgressEvent::InstallFinished { status } => {
            if let Some(footer) = footer_slot.lock().expect("footer lock poisoned").take() {
                footer.finish_with_status(match status {
                    ProgressFinishStatus::Done => FinishStatus::Done,
                    ProgressFinishStatus::Failed => FinishStatus::Failed,
                    ProgressFinishStatus::Interrupted => FinishStatus::Interrupted,
                });
            }
        }
    }
}

fn render_notice(
    footer_slot: &Mutex<Option<Arc<ProgressFooter>>>,
    stream: OutputStream,
    message: &str,
) {
    if message.trim().is_empty() {
        return;
    }
    if let Some(footer) = footer_slot.lock().expect("footer lock poisoned").as_ref() {
        footer.notice(stream, message);
        return;
    }
    match stream {
        OutputStream::Stdout => println!("{message}"),
        OutputStream::Stderr => eprintln!("{message}"),
    }
}

impl ExecutionEvents for HumanTtyExecutionEvents {
    fn progress(&self, event: ProgressEvent) {
        render_progress(&self.footer, true, event);
    }

    fn notice(&self, stream: OutputStream, message: &str) {
        render_notice(&self.footer, stream, message);
    }
}

impl ExecutionEvents for HumanPlainExecutionEvents {
    fn progress(&self, event: ProgressEvent) {
        render_progress(&self.footer, false, event);
    }

    fn notice(&self, stream: OutputStream, message: &str) {
        render_notice(&self.footer, stream, message);
    }
}
