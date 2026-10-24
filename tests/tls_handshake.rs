//! Проверка TLS до T-Invest после обновления gRPC-стека: запрос с заведомо
//! неверным токеном доходит до брокера и получает «не авторизован», а не
//! транспортную ошибку. Настоящий токен не нужен и не используется.

use secrecy::SecretString;
use tinkoff_rs::{SandboxApi, proto::GetInfoRequest};

#[tokio::test]
#[ignore = "network: reaches the T-Invest sandbox"]
async fn tls_handshake_reaches_the_broker() {
    let api = SandboxApi::builder(SecretString::new("not-a-token".to_owned()))
        .app_name("tinkoff-rs.tls-check")
        .connect()
        .await
        .expect("TLS connection to the sandbox endpoint");
    let status = api
        .raw()
        .users()
        .get_info(GetInfoRequest {})
        .await
        .expect_err("a fake token is never accepted");
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
}
