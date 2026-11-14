//! Prometheus-style counters derived from committed events.

use crate::model::Event;

#[derive(Debug, Default, Clone)]
pub struct Metrics {
    pub executions_started: u64,
    pub executions_completed: u64,
    pub executions_failed: u64,
    pub executions_cancelled: u64,
    pub executions_timed_out: u64,
    pub tasks_dispatched: u64,
    pub tasks_succeeded: u64,
    pub task_attempts_failed: u64,
    pub task_attempts_uncertain: u64,
    pub retries_scheduled: u64,
    pub steps_skipped: u64,
    pub stale_results: u64,
    pub compensations_started: u64,
    pub compensations_failed: u64,
    pub events_committed: u64,
}

impl Metrics {
    pub fn observe(&mut self, e: &Event) {
        self.events_committed += 1;
        match e {
            Event::WorkflowCreated { .. } => self.executions_started += 1,
            Event::WorkflowCompleted { .. } => self.executions_completed += 1,
            Event::WorkflowFailed { .. } => self.executions_failed += 1,
            Event::WorkflowCancelled { .. } => self.executions_cancelled += 1,
            Event::WorkflowTimedOut { .. } => self.executions_timed_out += 1,
            Event::StepDispatched { .. } => self.tasks_dispatched += 1,
            Event::StepSucceeded { .. } => self.tasks_succeeded += 1,
            Event::StepAttemptFailed { .. } => self.task_attempts_failed += 1,
            Event::StepAttemptUncertain { .. } => self.task_attempts_uncertain += 1,
            Event::RetryScheduled { .. } => self.retries_scheduled += 1,
            Event::StepSkipped { .. } => self.steps_skipped += 1,
            Event::StaleResultIgnored { .. } => self.stale_results += 1,
            Event::CompensationStarted { .. } => self.compensations_started += 1,
            Event::CompensationFailed { .. } => self.compensations_failed += 1,
            _ => {}
        }
    }

    pub fn render(&self, active: usize, sessions: usize) -> String {
        let counters = [
            (
                "orbit_executions_started_total",
                "Executions started",
                self.executions_started,
            ),
            (
                "orbit_executions_completed_total",
                "Executions completed",
                self.executions_completed,
            ),
            (
                "orbit_executions_failed_total",
                "Executions failed",
                self.executions_failed,
            ),
            (
                "orbit_executions_cancelled_total",
                "Executions cancelled",
                self.executions_cancelled,
            ),
            (
                "orbit_executions_timed_out_total",
                "Executions timed out",
                self.executions_timed_out,
            ),
            (
                "orbit_tasks_dispatched_total",
                "Task attempts dispatched",
                self.tasks_dispatched,
            ),
            (
                "orbit_tasks_succeeded_total",
                "Task attempts succeeded",
                self.tasks_succeeded,
            ),
            (
                "orbit_task_attempts_failed_total",
                "Task attempts failed",
                self.task_attempts_failed,
            ),
            (
                "orbit_task_attempts_uncertain_total",
                "Task attempts with unknown outcome",
                self.task_attempts_uncertain,
            ),
            (
                "orbit_retries_scheduled_total",
                "Retries scheduled",
                self.retries_scheduled,
            ),
            (
                "orbit_steps_skipped_total",
                "Steps skipped by conditions",
                self.steps_skipped,
            ),
            (
                "orbit_stale_results_total",
                "Duplicate or late results ignored",
                self.stale_results,
            ),
            (
                "orbit_compensations_started_total",
                "Compensations started",
                self.compensations_started,
            ),
            (
                "orbit_compensations_failed_total",
                "Compensations failed",
                self.compensations_failed,
            ),
            (
                "orbit_events_committed_total",
                "History events committed",
                self.events_committed,
            ),
        ];
        let mut out = String::new();
        for (name, help, value) in counters {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
            ));
        }
        out.push_str(&format!(
            "# HELP orbit_active_executions Executions not yet finished\n# TYPE orbit_active_executions gauge\norbit_active_executions {active}\n"
        ));
        out.push_str(&format!(
            "# HELP orbit_worker_sessions Registered worker sessions\n# TYPE orbit_worker_sessions gauge\norbit_worker_sessions {sessions}\n"
        ));
        out
    }
}
