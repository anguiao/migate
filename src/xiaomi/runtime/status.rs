use crate::device::PhysicalDeviceId;
use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
};

/// Publishes runtime observations without exposing their mutable containers.
pub(super) struct RuntimeStatus {
    current: RefCell<XiaomiRuntimeStatus>,
    diagnostics: RefCell<VecDeque<XiaomiRuntimeDiagnostic>>,
}

impl RuntimeStatus {
    pub(super) fn new(admission: super::AdmissionSnapshot) -> Self {
        Self {
            current: RefCell::new(XiaomiRuntimeStatus {
                admission,
                candidates: Vec::new(),
                gateways: Vec::new(),
                diagnostics: Vec::new(),
            }),
            diagnostics: RefCell::new(VecDeque::new()),
        }
    }

    pub(super) fn snapshot(&self) -> XiaomiRuntimeStatus {
        let mut status = self.current.borrow().clone();
        status.diagnostics = self.diagnostics.borrow().iter().cloned().collect();
        status
    }

    pub(super) fn admission(&self) -> super::AdmissionSnapshot {
        self.current.borrow().admission.clone()
    }

    pub(super) fn admitted_devices(&self) -> BTreeSet<PhysicalDeviceId> {
        self.current
            .borrow()
            .admission
            .features
            .iter()
            .map(|feature| feature.identity.physical.clone())
            .collect()
    }

    pub(super) fn set_admission(&self, admission: super::AdmissionSnapshot) {
        self.current.borrow_mut().admission = admission;
    }

    pub(super) fn set_devices(
        &self,
        candidates: Vec<XiaomiCandidateStatus>,
        gateways: Vec<XiaomiGatewayStatus>,
    ) {
        let mut status = self.current.borrow_mut();
        status.candidates = candidates;
        status.gateways = gateways;
    }

    pub(super) fn record_diagnostic(&self, diagnostic: XiaomiRuntimeDiagnostic) {
        let mut diagnostics = self.diagnostics.borrow_mut();
        if diagnostics
            .back()
            .is_some_and(|current| same_diagnostic(current, &diagnostic))
        {
            return;
        }
        if diagnostics.len() == 32 {
            diagnostics.pop_front();
        }
        log::warn!("{}", format_diagnostic(&diagnostic));
        diagnostics.push_back(diagnostic);
    }
}

#[derive(Clone, Debug)]
pub struct XiaomiRuntimeStatus {
    pub admission: super::AdmissionSnapshot,
    pub candidates: Vec<XiaomiCandidateStatus>,
    pub gateways: Vec<XiaomiGatewayStatus>,
    pub diagnostics: Vec<XiaomiRuntimeDiagnostic>,
}

#[derive(Clone, Debug)]
pub enum XiaomiRuntimeDiagnostic {
    Command(super::RuntimeDiagnostic),
    State(super::StateDiagnostic),
    Boundary(XiaomiBoundaryDiagnostic),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiBoundaryDiagnostic {
    pub component: XiaomiRuntimeComponent,
    pub stage: XiaomiFailureStage,
    pub code: XiaomiSafeFailureCode,
    pub subject: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiRuntimeComponent {
    Network,
    Catalog,
    Gateway,
    Lan,
    Cloud,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiFailureStage {
    Discover,
    Connect,
    Authenticate,
    Subscribe,
    Read,
    Write,
    Publish,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiSafeFailureCode {
    Unavailable,
    Timeout,
    Unauthorized,
    Protocol,
    Rejected(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiCandidateStatus {
    pub account_id: String,
    pub home_id: String,
    pub did: String,
    pub parent_did: String,
    pub name: String,
    pub model: String,
    pub state: XiaomiCandidateState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XiaomiCandidateState {
    Unsupported,
    Unrecognized,
    WaitingForGateway,
    Ready,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XiaomiGatewayStatus {
    pub did: u64,
    pub home_group: String,
    pub unverified: bool,
    pub authenticated: bool,
}

fn same_diagnostic(left: &XiaomiRuntimeDiagnostic, right: &XiaomiRuntimeDiagnostic) -> bool {
    match (left, right) {
        (XiaomiRuntimeDiagnostic::Command(left), XiaomiRuntimeDiagnostic::Command(right)) => {
            left == right
        }
        (XiaomiRuntimeDiagnostic::State(left), XiaomiRuntimeDiagnostic::State(right)) => {
            left == right
        }
        (XiaomiRuntimeDiagnostic::Boundary(left), XiaomiRuntimeDiagnostic::Boundary(right)) => {
            left == right
        }
        _ => false,
    }
}

fn format_diagnostic(diagnostic: &XiaomiRuntimeDiagnostic) -> String {
    match diagnostic {
        XiaomiRuntimeDiagnostic::Command(super::RuntimeDiagnostic::CommandFailure {
            feature,
            command,
            path,
            stage,
            outcome,
            sent,
        }) => format!(
            "Xiaomi command failed: device={} service={} role={} command={command:?} path={path:?} stage={stage:?} outcome={outcome:?} sent={sent}",
            feature.physical.parent_did,
            feature.service_instance,
            feature.role.as_str(),
        ),
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::ReadFailure {
            feature,
            path,
            failure,
        }) => format!(
            "Xiaomi state read failed: device={} service={} role={} path={path:?} outcome={failure:?}",
            feature.physical.parent_did,
            feature.service_instance,
            feature.role.as_str(),
        ),
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::QueueFull) => {
            "Xiaomi state refresh queue is full".to_owned()
        }
        XiaomiRuntimeDiagnostic::State(super::StateDiagnostic::StorageFailure(error)) => {
            format!("Xiaomi state persistence failed: {error}")
        }
        XiaomiRuntimeDiagnostic::Boundary(diagnostic) => format!(
            "Xiaomi boundary failure: component={:?} stage={:?} code={:?} subject={}",
            diagnostic.component,
            diagnostic.stage,
            diagnostic.code,
            diagnostic.subject.as_deref().unwrap_or("none"),
        ),
    }
}
