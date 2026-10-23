use super::*;
use crate::DestinationPolicy;
use crate::NetworkPolicyController;
use crate::NetworkPolicyDenied;
use crate::OutboundProxyPolicy;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::thread::JoinHandle;

struct Server {
    origin: String,
    roots: Vec<u8>,
    requests: JoinHandle<Vec<String>>,
}

// One accepted socket must carry every request, so a per-request client pool
// fails the second operation instead of passing as a count-only assertion.
fn server(responses: Vec<&'static str>) -> Server {
    server_with_body_barrier(responses, None)
}

fn server_with_body_barrier(
    responses: Vec<&'static str>,
    mut barrier: Option<std::sync::mpsc::Receiver<()>>,
) -> Server {
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let roots = certified.cert.der().to_vec();
    let config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certified.cert.der().clone()],
                certified.signing_key.into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let requests = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut tls =
            rustls::StreamOwned::new(rustls::ServerConnection::new(config).unwrap(), socket);
        let mut requests = Vec::new();
        for response in responses {
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if tls.read_exact(&mut byte).is_err() {
                    return requests;
                }
                bytes.push(byte[0]);
            }
            requests.push(String::from_utf8(bytes).unwrap());
            if let Some(barrier) = barrier.take() {
                let (headers, body) = response.split_once("\r\n\r\n").unwrap();
                tls.write_all(headers.as_bytes()).unwrap();
                tls.write_all(b"\r\n\r\n").unwrap();
                tls.flush().unwrap();
                barrier.recv_timeout(Duration::from_secs(5)).unwrap();
                // A timeout case intentionally drops the reader first.
                let _ = tls.write_all(body.as_bytes());
                let _ = tls.flush();
            } else {
                tls.write_all(response.as_bytes()).unwrap();
                tls.flush().unwrap();
            }
        }
        requests
    });
    Server {
        origin,
        roots,
        requests,
    }
}

#[tokio::test]
async fn streaming_budget_allows_delayed_body_and_reports_its_deadline_as_timeout() {
    for (elapsed_seconds, expired) in [(31, false), (181, true)] {
        let (release, barrier) = std::sync::mpsc::channel();
        let server = server_with_body_barrier(
            vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"],
            Some(barrier),
        );
        let client = client(&server, crate::NetworkPolicy::unmanaged()).await;
        let response = client
            .get(&server.origin)
            .timeout(Duration::from_secs(180))
            .send()
            .await
            .unwrap();
        // Complete the real TLS handshake before pausing Tokio's clock. The
        // server's barrier ensures the body is still pending at the deadline.
        tokio::time::pause();
        let mut body = Box::pin(response.bytes());
        assert!(body.as_mut().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(elapsed_seconds)).await;
        tokio::time::resume();
        if expired {
            let error = body.await.unwrap_err();
            assert!(error.is_timeout(), "timeout category was lost: {error:?}");
            assert!(!error.to_string().contains(&server.origin));
            release.send(()).unwrap();
        } else {
            assert!(body.as_mut().now_or_never().is_none());
            release.send(()).unwrap();
            assert_eq!(body.await.unwrap(), "ok");
        }
        assert_eq!(server.requests.join().unwrap().len(), 1);
    }
}

async fn client(server: &Server, policy: crate::NetworkPolicy) -> HttpClient {
    let factory =
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault).with_network_policy(policy);
    let client = factory
        .browser_chat_client(&server.origin, HeaderMap::new())
        .unwrap();
    let HttpClientBackend::BrowserChat(pool) = &client.backend else {
        unreachable!()
    };
    let route = factory
        .resolve_browser_chat_proxy_route(&server.origin)
        .await
        .unwrap();
    let roots = wreq::tls::trust::CertStore::builder()
        .add_der_cert(server.roots.as_slice())
        .build()
        .unwrap();
    // Hermetic TLS server; production root loading is covered separately.
    pool.clients.lock().await.insert(
        route,
        build_client_with_roots(&OutboundProxyRoute::Direct, roots).unwrap(),
    );
    client
}

#[tokio::test]
async fn persistent_tls_session_keeps_profile_and_per_request_credentials() {
    let server = server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ]);
    let client = client(&server, crate::NetworkPolicy::unmanaged()).await;
    let response = client.get(&server.origin).send().await.unwrap();
    assert_eq!(response.url().origin().ascii_serialization(), server.origin);
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(
        client
            .get(&server.origin)
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    let requests = server.requests.join().unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(request.contains("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36"));
        assert!(request.to_ascii_lowercase().contains("sec-ch-ua:"));
    }
    assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token")
    );
}

#[tokio::test]
async fn redirects_are_returned_and_do_not_forward_credentials() {
    // A relative target is enough: following it would create a second request
    // on the one TLS connection, and send would not return this 302.
    let server = server(vec![
        "HTTP/1.1 302 Found\r\nLocation: /private\r\nContent-Length: 0\r\n\r\n",
    ]);
    let client = client(&server, crate::NetworkPolicy::unmanaged()).await;
    assert_eq!(
        client
            .get(&server.origin)
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .status(),
        http::StatusCode::FOUND
    );
    assert_eq!(server.requests.join().unwrap().len(), 1);
}

#[tokio::test]
async fn policy_revocation_is_enforced_after_response_headers() {
    let server = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nsecret"]);
    let controller = NetworkPolicyController::default();
    let policy = controller.policy();
    controller.publish(policy.revision(), DestinationPolicy::Unrestricted);
    let client = client(&server, policy.clone()).await;
    let response = client.get(&server.origin).send().await.unwrap();
    policy.invalidate();
    assert!(matches!(
        response.bytes().await,
        Err(HttpError::Policy(NetworkPolicyDenied::Revoked))
    ));
    assert_eq!(server.requests.join().unwrap().len(), 1);
}

#[tokio::test]
async fn unavailable_policy_and_cross_origin_requests_never_connect() {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let origin = format!("https://{}", socket.local_addr().unwrap());
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
        .with_network_policy(NetworkPolicyController::default().policy());
    let client = factory
        .browser_chat_client(&origin, HeaderMap::new())
        .unwrap();
    assert!(matches!(
        client.get(&origin).send().await,
        Err(HttpError::Policy(NetworkPolicyDenied::Unavailable))
    ));
    assert!(matches!(
        client.get("https://example.com/private").send().await,
        Err(HttpError::Build(_))
    ));
    assert_eq!(
        socket.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn untrusted_tls_fails_without_another_backend() {
    let server = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]);
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let roots = wreq::tls::trust::CertStore::builder()
        .add_der_cert(other.cert.der().as_ref())
        .build()
        .unwrap();
    let client = build_client_with_roots(&OutboundProxyRoute::Direct, roots).unwrap();
    assert!(client.get(&server.origin).send().await.is_err());
    assert!(server.requests.join().unwrap().is_empty());
}

#[tokio::test]
async fn trusted_certificate_still_requires_a_matching_hostname() {
    let server = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]);
    let roots = wreq::tls::trust::CertStore::builder()
        .add_der_cert(server.roots.as_slice())
        .build()
        .unwrap();
    let client = build_client_with_roots(&OutboundProxyRoute::Direct, roots).unwrap();
    assert!(
        client
            .get(server.origin.replace("localhost", "127.0.0.1"))
            .send()
            .await
            .is_err()
    );
    assert!(server.requests.join().unwrap().is_empty());
}

#[tokio::test]
async fn server_failure_does_not_replay_a_post() {
    let server = server(vec![
        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n",
    ]);
    let client = client(&server, crate::NetworkPolicy::unmanaged()).await;
    assert_eq!(
        client.post(&server.origin).send().await.unwrap().status(),
        http::StatusCode::SERVICE_UNAVAILABLE
    );
    let requests = server.requests.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST "));
}

#[tokio::test]
async fn https_connect_proxy_keeps_chat_credentials_inside_tls() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let server = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let target_port = reqwest::Url::parse(&server.origin).unwrap().port().unwrap();
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let tunnel = tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(socket.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        let mut target = tokio::net::TcpStream::connect(("127.0.0.1", target_port))
            .await
            .unwrap();
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        // TLS close without close_notify can end either side with an IO error.
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut target).await;
        String::from_utf8(headers).unwrap()
    });
    let roots = wreq::tls::trust::CertStore::builder()
        .add_der_cert(server.roots.as_slice())
        .build()
        .unwrap();
    let client = build_client_with_roots(
        &OutboundProxyRoute::Proxy {
            url: proxy_url,
            no_proxy: None,
        },
        roots,
    )
    .unwrap();
    let response = client
        .get(&server.origin)
        .bearer_auth("chat-test-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "ok");
    drop(client);
    let connect = tokio::time::timeout(Duration::from_secs(5), tunnel)
        .await
        .unwrap()
        .unwrap();
    assert!(connect.starts_with(&format!("CONNECT localhost:{target_port} ")));
    assert!(!connect.contains("chat-test-secret"));
    assert!(server.requests.join().unwrap()[0].contains("chat-test-secret"));
}

#[tokio::test]
async fn unavailable_proxy_never_falls_back_to_direct() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    drop(proxy);
    let roots = wreq::tls::trust::CertStore::builder()
        .add_pem_cert(include_bytes!("../tests/fixtures/test-ca.pem").as_slice())
        .build()
        .unwrap();
    let client = build_client_with_roots(
        &OutboundProxyRoute::Proxy {
            url: proxy_url,
            no_proxy: None,
        },
        roots,
    )
    .unwrap();
    assert!(
        client
            .get(format!("https://{}", target.local_addr().unwrap()))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn invalid_proxy_and_session_configuration_fail_explicitly() {
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    for origin in [
        "http://chatgpt.com",
        "https://user:password@chatgpt.com",
        "https://chatgpt.com/path",
        "https://chatgpt.com/?secret",
    ] {
        assert!(
            factory
                .browser_chat_client(origin, HeaderMap::new())
                .is_err()
        );
    }
    let roots = wreq::tls::trust::CertStore::builder()
        .add_pem_cert(include_bytes!("../tests/fixtures/test-ca.pem").as_slice())
        .build()
        .unwrap();
    let result = build_client_with_roots(
        &OutboundProxyRoute::Proxy {
            url: "invalid://private-password".into(),
            no_proxy: None,
        },
        roots,
    );
    assert!(result.is_err());
    assert!(!format!("{:?}", result.err().unwrap()).contains("private-password"));
    for name in [
        "authorization",
        "x-conduit-token",
        "openai-sentinel-proof-token",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(name, "secret".parse().unwrap());
        assert!(
            factory
                .browser_chat_client("https://chatgpt.com", headers)
                .is_err()
        );
    }
}
