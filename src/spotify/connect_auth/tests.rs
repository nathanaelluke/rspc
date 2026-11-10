use super::*;
use protobuf::MessageField;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

fn info() -> proto::AuthInfo {
    proto::AuthInfo {
        id: "678434dc-fd33-4b77-9822-d8b252ae25e9".into(),
        client_id: "receiver-client".into(),
        method: MessageField::some(proto::AuthInfoMethod {
            dpop_bound_auth_code: MessageField::some(proto::DpopBoundAuthCodeInfo {
                jkt: "a".repeat(43),
                pkce_code_challenge: "b".repeat(43),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn receiver(info: &proto::AuthInfo) -> Receiver {
    Receiver {
        status: 101,
        device_id: "test-device".into(),
        active_user: String::new(),
        auth_info: STANDARD.encode(info.write_to_bytes().unwrap()),
    }
}

#[test]
fn receiver_identity_and_both_key_bindings_are_required() {
    let mut info = info();
    assert!(receiver(&info).authorization("test-device").is_ok());
    assert!(receiver(&info).authorization("another-device").is_err());
    info.id.clear();
    assert!(receiver(&info).authorization("test-device").is_err());
    info = self::info();
    info.method
        .mut_or_insert_default()
        .dpop_bound_auth_code
        .mut_or_insert_default()
        .jkt = "bad".into();
    assert!(receiver(&info).authorization("test-device").is_err());
    info = self::info();
    info.method
        .mut_or_insert_default()
        .dpop_bound_auth_code
        .mut_or_insert_default()
        .pkce_code_challenge = "!".repeat(43);
    assert!(receiver(&info).authorization("test-device").is_err());
    info.method.clear();
    assert!(receiver(&info).authorization("test-device").is_err());
}

#[test]
fn a_different_paired_account_is_rejected_before_requesting_a_code() {
    let mut receiver = receiver(&info());
    assert!(receiver.check_account("catalog-user").is_ok());
    receiver.active_user = "catalog-user".into();
    assert!(receiver.check_account("catalog-user").is_ok());
    assert!(receiver.check_account("another-user").is_err());
}

#[test]
fn receiver_messages_have_a_bounded_size() {
    let mut receiver = receiver(&info());
    receiver.auth_info = "A".repeat(16 * 1024 + 1);
    assert!(receiver.authorization("test-device").is_err());
}

async fn server(status: u16, body: &str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let body = body.to_owned();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.trim().split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload).await.unwrap();
        reader.get_mut().write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        String::from_utf8(payload).unwrap()
    });
    (endpoint, task)
}

#[tokio::test]
async fn the_exchange_binds_the_code_to_the_receiver_without_forwarding_the_account_grant() {
    let info = info();
    let (endpoint, task) = server(
        200,
        &format!(r#"{{"access_token":"one-use-code","issued_token_type":"{CODE_TYPE}"}}"#),
    )
    .await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let blob = exchange(
        &http,
        &endpoint,
        "catalog-client",
        "account-token",
        "client-token",
        &info,
    )
    .await
    .unwrap();
    let request: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(task.await.unwrap().as_bytes())
            .into_owned()
            .collect();
    assert_eq!(request["client_id"], "catalog-client");
    assert_eq!(request["audience"], "receiver-client");
    assert_eq!(request["subject_token"], "account-token");
    assert_eq!(request["resource"], "urn:spotify:resources:connect");
    assert_eq!(request["dpop_jkt"], "a".repeat(43));
    assert_eq!(request["code_challenge"], "b".repeat(43));
    assert_eq!(request["code_challenge_method"], "S256");
    let wire = STANDARD.decode(blob.as_str()).unwrap();
    assert!(
        !wire
            .windows(b"account-token".len())
            .any(|w| w == b"account-token")
    );
    let token = proto::AuthToken::parse_from_bytes(&wire).unwrap();
    assert_eq!(token.auth_info_id, info.id);
    assert!(
        matches!(token.token, Some(proto::auth_token::Token::DpopBoundAuthCode(code)) if code.value == "one-use-code")
    );
    // The receiver's union field is 10, not field 2; a wrong field gets an
    // acknowledgement but cannot create an authenticated session.
    assert!(wire.contains(&0x52));
}

#[tokio::test]
async fn refusal_does_not_echo_credentials_or_retry_the_exchange() {
    let (endpoint, task) = server(403, r#"{"error_description":"private-account-token"}"#).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let error = exchange(
        &http,
        &endpoint,
        "client",
        "private-account-token",
        "client-token",
        &info(),
    )
    .await
    .unwrap_err();
    assert!(!format!("{error:#}").contains("private-account-token"));
    assert!(format!("{error:#}").contains("403"));
    task.await.unwrap();
}

#[tokio::test]
async fn an_access_token_cannot_be_mistaken_for_a_connect_authorization_code() {
    let (endpoint, task) = server(200, r#"{"access_token":"unit-access","issued_token_type":"urn:ietf:params:oauth:token-type:access_token"}"#).await;
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    assert!(
        exchange(
            &http,
            &endpoint,
            "client",
            "account-token",
            "client-token",
            &info()
        )
        .await
        .is_err()
    );
    task.await.unwrap();
}
