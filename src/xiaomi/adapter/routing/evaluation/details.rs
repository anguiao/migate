//! Materialize unavailable reasons only for detailed consumers.

use super::core::{Core, Unavailable};
use crate::xiaomi::adapter::model::LanState;

impl Core<'_> {
    pub(super) fn describe<T: Clone>(
        &self,
        did: &str,
        candidate: &Result<T, Unavailable>,
    ) -> Result<T, String> {
        candidate
            .clone()
            .map_err(|failure| self.reason(did, failure))
    }

    fn reason(&self, did: &str, failure: Unavailable) -> String {
        match failure {
            Unavailable::Fact(reason) => reason.into(),
            Unavailable::Certificate => "gateway certificate not currently valid".into(),
            Unavailable::Authentication(failure) => failure.reason().into(),
            Unavailable::HubAccess => match self.hub.access(did).unwrap() {
                Err(error) => error.to_string(),
                Ok(_) => unreachable!("captured hub access failure"),
            },
            Unavailable::DirectNotReady => self.directory.devices[did]
                .work
                .as_ref()
                .unwrap()
                .lan
                .borrow()
                .status
                .as_ref()
                .map_or_else(
                    || "not yet initialized".into(),
                    |status| {
                        status.problem.clone().unwrap_or_else(|| {
                            match status.state {
                                LanState::Ineligible => "device is not an eligible IP device",
                                LanState::MissingCredentials => "credentials missing",
                                LanState::InvalidCredentials => "credentials invalid",
                                LanState::Authenticating => "authentication in progress",
                                LanState::Ready => "session unavailable",
                                LanState::Failed => "authentication or communication failed",
                            }
                            .into()
                        })
                    },
                ),
        }
    }
}
