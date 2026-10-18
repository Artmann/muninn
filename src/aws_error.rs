//! Turning AWS SDK errors into messages a person can act on.

use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;

/// What to grant when the document bucket denies access.
pub fn s3_permissions(bucket: &str) -> String {
    format!(
        "s3:ListBucket on arn:aws:s3:::{bucket} and s3:GetObject and s3:PutObject on \
         arn:aws:s3:::{bucket}/*"
    )
}

/// What to grant when the vector bucket denies access.
pub fn s3vectors_permissions(region: &str, bucket: &str) -> String {
    format!(
        "s3vectors:GetIndex, CreateIndex, PutVectors, QueryVectors and GetVectors on \
         arn:aws:s3vectors:{region}:<account-id>:bucket/{bucket} and \
         arn:aws:s3vectors:{region}:<account-id>:bucket/{bucket}/*"
    )
}

/// Attach `doing` to an SDK error. An access-denied error becomes a single
/// message naming the permissions to grant, followed by AWS's own reason.
pub fn explain<E, R>(err: SdkError<E, R>, doing: String, permissions: &str) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    if matches!(err.code(), Some("AccessDenied" | "AccessDeniedException")) {
        let reason = err.message().unwrap_or("no reason given").to_string();
        return anyhow::anyhow!(
            "access denied while {doing}. The AWS credentials need {permissions}; \
             see \"IAM permissions\" in the README. AWS said: {reason}"
        );
    }
    anyhow::Error::new(err).context(doing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::error::ErrorMetadata;

    #[derive(Debug)]
    struct Fake(ErrorMetadata);

    impl std::fmt::Display for Fake {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fake service error")
        }
    }
    impl std::error::Error for Fake {}
    impl ProvideErrorMetadata for Fake {
        fn meta(&self) -> &ErrorMetadata {
            &self.0
        }
    }

    fn service_error(code: &str, message: &str) -> SdkError<Fake, ()> {
        SdkError::service_error(
            Fake(ErrorMetadata::builder().code(code).message(message).build()),
            (),
        )
    }

    #[test]
    fn access_denied_names_permissions_and_aws_reason() {
        for code in ["AccessDenied", "AccessDeniedException"] {
            let err = explain(
                service_error(code, "User: arn:aws:iam::1:user/x is not authorized"),
                "reading index b/i".into(),
                &s3vectors_permissions("eu-west-3", "b"),
            );
            let text = format!("{err:#}");
            assert!(
                text.contains("access denied while reading index b/i"),
                "{text}"
            );
            assert!(text.contains("s3vectors:GetIndex"), "{text}");
            assert!(
                text.contains("arn:aws:s3vectors:eu-west-3:<account-id>:bucket/b/*"),
                "{text}"
            );
            assert!(text.contains("user/x is not authorized"), "{text}");
            assert_eq!(err.chain().count(), 1, "one message, no repeated causes");
        }
    }

    #[test]
    fn other_errors_keep_their_cause() {
        let err = explain(
            service_error("NoSuchBucket", "gone"),
            "listing objects".into(),
            &s3_permissions("b"),
        );
        assert_eq!(err.to_string(), "listing objects");
        assert!(!format!("{err:#}").contains("s3:ListBucket"));
    }
}
