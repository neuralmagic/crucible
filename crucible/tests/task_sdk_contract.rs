//! The task SDK and the engine agree on how a task finds its inputs.

#[test]
fn the_sdk_reads_what_the_engine_sets() {
    assert_eq!(
        crucible_task::INPUTS_FILE_ENV,
        crucible::plan::runner::INPUTS_FILE_ENV
    );
    assert_eq!(crucible_task::TASK_NAME_ENV, crucible::plan::TASK_NAME_ENV);
    assert_eq!(
        crucible_task::PARAMS_INPUT,
        crucible::plan::ir::PARAMS_INPUT
    );
    assert_eq!(crucible_task::STAGED_INPUTS, crucible::plan::STAGED_INPUTS);
}
