use enzimos_phone_client::{ControlRequest, request};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn private_socket_roundtrip_refuses_unsafe_endpoints_and_oversized_replies() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.path().join("control.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        request(&path, &ControlRequest::Devices)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        request(&path, &ControlRequest::Devices)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = tokio::spawn(async move {
        for response in [
            "{\"ok\":true,\"devices\":[]}\n".to_owned(),
            format!("{{\"ok\":true,\"data\":\"{}\"}}\n", "x".repeat(262123)),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["cmd"],
                "devices"
            );
            let _ = stream.get_mut().write_all(response.as_bytes()).await;
        }
    });
    assert_eq!(
        request(&path, &ControlRequest::Devices).await.unwrap().0["devices"],
        serde_json::json!([])
    );
    assert_eq!(
        request(&path, &ControlRequest::Devices)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    server.await.unwrap();
}
