//! The cluster dashboard's part of the visible view tree ([`crate::ui::view_state`]): the activity
//! feed's rows.

// module-tests: dashboard_tests.rs

use super::super::view_state::{DashboardState, WarningRowState};
use super::{DashboardModel, DashboardView, WarningsPanel};

impl DashboardView {
    pub(in crate::ui) fn view_state(&self) -> DashboardState {
        self.model.view_state()
    }
}

impl DashboardModel {
    pub(in crate::ui) fn view_state(&self) -> DashboardState {
        let (available, total, rows) = match &self.warnings {
            WarningsPanel::Unavailable => (false, 0, &[][..]),
            WarningsPanel::Rows { rows, total } => (true, *total, rows.as_slice()),
        };
        DashboardState {
            warnings_available: available,
            warnings_total: total,
            warnings: rows
                .iter()
                .map(|row| WarningRowState {
                    id: format!("dashboard-warning-{}", row.key),
                    age: row.age.clone(),
                    reason: row.reason.clone(),
                    object: row.object.clone(),
                    kind: row.open.as_ref().map(|open| open.kind.clone()),
                    namespace: row.open.as_ref().and_then(|open| open.namespace.clone()),
                    name: row.open.as_ref().map(|open| open.name.clone()),
                    message: row.message.clone(),
                    occurrences: row.occurrences,
                    opens: row.open.is_some(),
                    rollout: row.rollout.map(|state| state.label()),
                })
                .collect(),
        }
    }
}
