//! Query bounds, typed request errors and the retry policy for idempotent reads.
use librespot_core::{Error, error::ErrorKind};
use reqwest::StatusCode;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "Spotify {}: {}", self.status.as_u16(), self.message)
    }
}
impl std::error::Error for ApiError {}
pub fn failure(status: StatusCode, message: impl Into<String>) -> anyhow::Error {
    ApiError {
        status,
        message: message.into(),
    }
    .into()
}
pub fn upstream(error: Error) -> anyhow::Error {
    let status = match error.kind {
        ErrorKind::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::InvalidArgument | ErrorKind::FailedPrecondition => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    failure(status, error.to_string())
}

pub fn parameter<'a>(query: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find_map(|(key, value)| (*key == name).then_some(value.as_str()))
}
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub offset: usize,
    pub limit: usize,
}
impl Window {
    pub fn query(query: &[(&str, String)], fallback: usize) -> Self {
        let offset = parameter(query, "offset")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let limit = parameter(query, "limit")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(fallback)
            .clamp(1, 1000);
        Self { offset, limit }
    }
    pub fn select<T>(self, values: &[T]) -> &[T] {
        values
            .get(self.offset..self.offset.saturating_add(self.limit).min(values.len()))
            .unwrap_or(&[])
    }
}

pub async fn read<T, F, Fut>(mut operation: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    let mut retries = 0;
    loop {
        let result = operation().await;
        if !matches!(&result,Err(error) if error.kind == ErrorKind::Unavailable || error.kind == ErrorKind::Internal)
            || retries == 2
        {
            return result;
        }
        retries += 1;
        tokio::time::sleep(std::time::Duration::from_millis(200 * retries)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pagination_is_bounded_and_overflow_safe() {
        let window = Window::query(
            &[
                ("limit", "99999999".into()),
                ("offset", usize::MAX.to_string()),
            ],
            20,
        );
        assert_eq!(window.limit, 1000);
        assert!(window.select(&[1, 2, 3]).is_empty());
        assert_eq!(Window::query(&[("limit", "0".into())], 20).limit, 1);
    }
    #[tokio::test]
    async fn limits_and_authentication_failures_are_not_retried() {
        for kind in [
            ErrorKind::ResourceExhausted,
            ErrorKind::PermissionDenied,
            ErrorKind::Unauthenticated,
        ] {
            let mut count = 0;
            let _: Result<(), _> = read(|| {
                count += 1;
                std::future::ready(Err(Error::new(kind, std::io::Error::other("fixture"))))
            })
            .await;
            assert_eq!(count, 1);
        }
    }
    #[test]
    fn status_survives_an_error_context() {
        let error = failure(StatusCode::TOO_MANY_REQUESTS, "limited").context("library");
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().status,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
