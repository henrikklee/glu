use crate::{command_model::CliError, CliFailure};

/// Apply the shared approval policy after a command has decided its computed
/// plan is broad enough to require confirmation.
///
/// `--yes` approves without constructing an error or touching the terminal.
/// JSON returns the command-owned typed confirmation error. Human mode runs
/// the command-owned prompt and treats a negative answer as normal
/// cancellation.
pub(crate) fn approve(
    globals: crate::command_model::GlobalOptions,
    json_error: impl FnOnce() -> CliError,
    prompt: impl FnOnce() -> anyhow::Result<bool>,
) -> Result<bool, CliFailure> {
    if globals.yes {
        return Ok(true);
    }
    if globals.is_json() {
        return Err(CliFailure::Structured(Box::new(json_error())));
    }
    prompt().map_err(CliFailure::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_model::{GlobalOptions, OutputFormat};

    fn confirmation_error() -> CliError {
        CliError::confirmation_required("approval required", Vec::new(), None)
    }

    #[test]
    fn yes_bypasses_error_construction_and_prompting() {
        let globals = GlobalOptions {
            yes: true,
            ..Default::default()
        };
        let approved = approve(
            globals,
            || panic!("must not construct an error"),
            || panic!("must not prompt"),
        )
        .unwrap();
        assert!(approved);
    }

    #[test]
    fn json_returns_the_typed_error_without_prompting() {
        let globals = GlobalOptions {
            output: OutputFormat::Json,
            ..Default::default()
        };
        let error = approve(globals, confirmation_error, || panic!("must not prompt"))
            .expect_err("JSON approval should fail");
        assert!(matches!(error, CliFailure::Structured(_)));
    }

    #[test]
    fn human_negative_answer_is_normal_cancellation() {
        let approved = approve(GlobalOptions::default(), confirmation_error, || Ok(false)).unwrap();
        assert!(!approved);
    }
}
