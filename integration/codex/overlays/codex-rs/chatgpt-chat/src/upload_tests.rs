use super::*;
use crate::attachment::AttachmentStager;
use crate::upload::UploadStage;
use base64::Engine;
use codex_http_client::HttpClientTlsConfig;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::io::Read;
use std::io::Write;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn native_upload_uses_isolated_tls_storage_and_requires_processing_completion() {
    for (status, completed) in [(201, true), (307, true), (201, false)] {
        codex_utils_rustls_provider::ensure_rustls_crypto_provider();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let roots = certified.cert.pem();
        let tls = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certified.cert.der().clone()],
                    certified.signing_key.into(),
                )
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let storage_url = format!(
            "https://localhost:{}/blob?sig=private-signed-value",
            listener.local_addr().unwrap().port()
        );
        let received = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut stream =
                rustls::StreamOwned::new(rustls::ServerConnection::new(tls).unwrap(), socket);
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                stream.read_exact(&mut b).unwrap();
                headers.push(b[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: 0\r\nLocation: https://must-not-follow.invalid/\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
            stream.flush().unwrap();
            (headers, body)
        });
        let server = MockServer::start().await;
        let claims = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}).to_string(),
        );
        let auth = CodexAuth::from_external_chatgpt_tokens(
            &format!("header.{claims}.signature"),
            "account_id",
            None,
        )
        .unwrap();
        let mut probe =
            ChatProbe::new(AuthManager::from_auth_for_testing(auth), true, None).unwrap();
        probe.backend = Url::parse(&format!("{}/backend-api/", server.uri())).unwrap();
        // Construct the session before replacing only its storage TLS roots for
        // this hermetic test. Production has no insecure-certificate switch.
        let _ = probe
            .request(Method::GET, "models", "models")
            .await
            .unwrap();
        probe.session.lock().unwrap().as_mut().unwrap().storage = HttpClientBuilder::new()
            .without_request_logging()
            .without_redirects()
            .without_automatic_retries()
            .build_direct_with_tls(
                HttpClientTlsConfig::default()
                    .with_root_certificate_pem(roots.as_bytes())
                    .unwrap(),
            )
            .unwrap();
        Mock::given(method("POST"))
            .and(path("/backend-api/files"))
            .respond_with(move |request: &wiremock::Request| {
                assert!(request.headers.contains_key("authorization"));
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["use_case"], "ace_upload");
                assert_eq!(body["store_in_library"], false);
                ResponseTemplate::new(200).set_body_json(
                    json!({"status":"success","file_id":"file-fixture","upload_url":storage_url}),
                )
            })
            .expect(1)
            .mount(&server)
            .await;
        let events = format!(
            "{}\n{}",
            json!({"event":"file.processing.file_ready","file_id":"file-fixture"}),
            if completed {
                json!({"event":"file.processing.completed","file_id":"file-fixture"}).to_string()
            } else {
                String::new()
            }
        );
        Mock::given(method("POST"))
            .and(path("/backend-api/files/process_upload_stream"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["metadata"]["is_temporary_chat"], true);
                assert_eq!(body["metadata"]["store_in_library"], false);
                assert_eq!(body["index_for_retrieval"], false);
                ResponseTemplate::new(200).set_body_raw(events.clone(), "application/x-ndjson")
            })
            .expect(if status == 201 { 1 } else { 0 })
            .mount(&server)
            .await;
        let path = std::env::temp_dir().join(format!("chat-upload-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"distinctive fixture bytes").unwrap();
        let files = AttachmentStager::new()
            .snapshot(vec![path.clone()], CancellationToken::new())
            .await
            .unwrap();
        let draft = uuid::Uuid::new_v4();
        let mut stages = Vec::new();
        let (ready, observation) = probe
            .upload_attachment(draft, true, &files[0], &CancellationToken::new(), |s, _| {
                stages.push(s)
            })
            .await;
        let (headers, body) = received.join().unwrap();
        assert_eq!(body, b"distinctive fixture bytes");
        for forbidden in [
            "authorization:",
            "cookie:",
            "chatgpt-",
            "sentinel",
            "oai-",
            "x-openai-",
            "referer:",
        ] {
            assert!(!headers.to_lowercase().contains(forbidden));
        }
        assert_eq!(observation.ready, status == 201 && completed);
        if let Some(ready) = ready {
            assert_eq!(ready.validate(&probe, draft, true), Ok(()));
            assert_eq!(
                ready.validate(&probe, uuid::Uuid::new_v4(), true),
                Err(ChatError::StaleUpload)
            );
            assert_eq!(
                ready.validate(&probe, draft, false),
                Err(ChatError::StaleUpload)
            );
            assert_eq!(stages.last(), Some(&UploadStage::Ready));
            probe.clear_session();
            let _request = probe.bootstrap_request().await.unwrap();
            assert_eq!(
                ready.validate(&probe, draft, true),
                Err(ChatError::StaleUpload)
            );
            probe.stop();
            assert_eq!(
                ready.validate(&probe, draft, true),
                Err(ChatError::LocalCancellation)
            );
        } else {
            assert!(!stages.contains(&UploadStage::Ready));
        }
        let report = serde_json::to_string(&observation).unwrap();
        for secret in [
            "private-signed-value",
            "file-fixture",
            "distinctive",
            "localhost",
        ] {
            assert!(!report.contains(secret));
        }
        std::fs::remove_file(path).unwrap();
    }
}
