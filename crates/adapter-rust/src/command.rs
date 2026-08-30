use std::process::Command;

use reporigor_process_tree::BoundedRunStage;
pub(crate) use reporigor_process_tree::{BoundedOutput, CommandLimits};

pub(crate) fn run_bounded(
    command: &mut Command,
    action: &str,
    limits: CommandLimits,
) -> Result<BoundedOutput, String> {
    reporigor_process_tree::run_bounded(command, limits).map_err(|error| match error.stage() {
        BoundedRunStage::Start => format!("cannot start {action}: {}", error.detail()),
        BoundedRunStage::Wait => format!("cannot wait for {action}: {}", error.detail()),
        BoundedRunStage::Output => format!("cannot collect {action} output: {}", error.detail()),
        BoundedRunStage::Timeout => format!(
            "{action} timed out after {:.3} seconds; the Cargo process tree was terminated",
            limits.timeout.as_secs_f64()
        ),
    })
}
