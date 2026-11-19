//! Classification of Kubernetes API responses, shared by the request paths in the core.

/// The API server's status for a kube error. Transport and client errors have no API status.
pub(crate) struct ApiError<'a> {
    pub code: u16,
    pub reason: &'a str,
    pub message: &'a str,
}

impl<'a> ApiError<'a> {
    pub fn from_kube(err: &'a kube::Error) -> Option<Self> {
        let kube::Error::Api(status) = err else {
            return None;
        };
        Some(Self {
            code: status.code,
            reason: &status.reason,
            message: &status.message,
        })
    }

    pub fn kind(&self) -> ApiErrorKind {
        ApiErrorKind::from_code(self.code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiErrorKind {
    BadRequest,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Gone,
    UnprocessableEntity,
    Other,
}

impl ApiErrorKind {
    /// Also classifies status objects carried in a successful list or watch response.
    pub fn from_code(code: u16) -> Self {
        match code {
            400 => Self::BadRequest,
            403 => Self::Forbidden,
            404 => Self::NotFound,
            405 => Self::MethodNotAllowed,
            410 => Self::Gone,
            422 => Self::UnprocessableEntity,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_api_errors_and_keeps_the_server_details() {
        for (code, kind) in [
            (400, ApiErrorKind::BadRequest),
            (403, ApiErrorKind::Forbidden),
            (404, ApiErrorKind::NotFound),
            (405, ApiErrorKind::MethodNotAllowed),
            (410, ApiErrorKind::Gone),
            (422, ApiErrorKind::UnprocessableEntity),
            (500, ApiErrorKind::Other),
        ] {
            let err = kube::Error::Api(Box::new(kube::core::Status {
                code,
                reason: "Reason".into(),
                message: "server details".into(),
                ..Default::default()
            }));
            let api = ApiError::from_kube(&err).unwrap();
            assert_eq!(api.kind(), kind);
            assert_eq!(api.code, code);
            assert_eq!(api.reason, "Reason");
            assert_eq!(api.message, "server details");
        }
    }
}
