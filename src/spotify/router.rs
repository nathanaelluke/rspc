use reqwest::StatusCode;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spotify {}: {}", self.status.as_u16(), self.message)
    }
}

impl std::error::Error for ApiError {}
/// `offset` and `limit` from the query, with the Web API's defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paging {
    pub offset: usize,
    pub limit: usize,
}

impl Paging {
    pub fn from_query(query: &[(&str, String)], default_limit: usize) -> Self {
        let number = |key| query_value(query, key).and_then(|value| value.parse().ok());
        Self {
            offset: number("offset").unwrap_or(0),
            limit: number("limit").unwrap_or(default_limit).max(1),
        }
    }

    pub fn window<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let start = self.offset.min(items.len());
        let end = start.saturating_add(self.limit).min(items.len());
        &items[start..end]
    }
}

pub fn query_value<'a>(query: &'a [(&str, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.as_str())
}

/// Runs an idempotent read again after a 5xx answer, twice at most.
pub(crate) async fn retry_read<T, F, Fut>(mut read: F) -> Result<T, librespot_core::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, librespot_core::Error>>,
{
    let mut attempt = 0;
    loop {
        match read().await {
            Err(error) if attempt < 2 && is_server_error(&error) => {
                attempt += 1;
                log::debug!("Spotify read failed ({error}); retry {attempt}");
                tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
            }
            result => return result,
        }
    }
}

fn is_server_error(error: &librespot_core::Error) -> bool {
    let text = error.to_string();
    ["500 ", "502 ", "503 ", "504 "]
        .iter()
        .any(|code| text.contains(code))
}

pub(crate) fn upstream_error(error: librespot_core::Error) -> anyhow::Error {
    use librespot_core::error::ErrorKind;
    let status = match error.kind {
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::ResourceExhausted => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::InvalidArgument | ErrorKind::FailedPrecondition => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    api_error(status, error.to_string())
}

pub(crate) fn api_error(status: StatusCode, body: impl Into<String>) -> anyhow::Error {
    ApiError {
        status,
        message: body.into(),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_survive_error_context_without_parsing_messages() {
        let error =
            api_error(StatusCode::TOO_MANY_REQUESTS, "request refused").context("catalog read");
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().status,
            StatusCode::TOO_MANY_REQUESTS
        );
        let unrelated = api_error(StatusCode::NOT_FOUND, "album 429 is unavailable");
        assert_eq!(
            unrelated.downcast_ref::<ApiError>().unwrap().status,
            StatusCode::NOT_FOUND
        );
    }
}
