use super::*;
use std::io::ErrorKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_configure_socket() {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("bind failed: {e}"),
    };
    let addr = listener.local_addr().unwrap();

    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("connect failed: {e}"),
    };
    if let Err(e) = configure_tcp_socket(&stream, true, Duration::from_secs(30)) {
        if e.kind() == ErrorKind::PermissionDenied {
            return;
        }
        panic!("configure_tcp_socket failed: {e}");
    }
}

#[tokio::test]
async fn test_configure_client_socket() {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("bind failed: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => panic!("local_addr failed: {e}"),
    };

    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("connect failed: {e}"),
    };

    if let Err(e) = configure_client_socket(&stream, 30, 30) {
        if e.kind() == ErrorKind::PermissionDenied {
            return;
        }
        panic!("configure_client_socket failed: {e}");
    }
}

#[tokio::test]
async fn test_configure_client_socket_zero_ack_timeout() {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("bind failed: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => panic!("local_addr failed: {e}"),
    };

    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("connect failed: {e}"),
    };

    if let Err(e) = configure_client_socket(&stream, 30, 0) {
        if e.kind() == ErrorKind::PermissionDenied {
            return;
        }
        panic!("configure_client_socket with zero ack timeout failed: {e}");
    }
}

#[tokio::test]
async fn test_configure_client_socket_roundtrip_io() {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("bind failed: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => panic!("local_addr failed: {e}"),
    };

    let server_task = tokio::spawn(async move {
        let (mut accepted, _) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => panic!("accept failed: {e}"),
        };
        let mut payload = [0u8; 4];
        if let Err(e) = accepted.read_exact(&mut payload).await {
            panic!("server read_exact failed: {e}");
        }
        if let Err(e) = accepted.write_all(b"pong").await {
            panic!("server write_all failed: {e}");
        }
        payload
    });

    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("connect failed: {e}"),
    };

    if let Err(e) = configure_client_socket(&stream, 30, 30) {
        if e.kind() == ErrorKind::PermissionDenied {
            return;
        }
        panic!("configure_client_socket failed: {e}");
    }

    if let Err(e) = stream.write_all(b"ping").await {
        panic!("client write_all failed: {e}");
    }

    let mut reply = [0u8; 4];
    if let Err(e) = stream.read_exact(&mut reply).await {
        panic!("client read_exact failed: {e}");
    }
    assert_eq!(&reply, b"pong");

    let server_seen = match server_task.await {
        Ok(value) => value,
        Err(e) => panic!("server task join failed: {e}"),
    };
    assert_eq!(&server_seen, b"ping");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_configure_client_socket_ack_timeout_overflow_rejected() {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("bind failed: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(e) => panic!("local_addr failed: {e}"),
    };

    let stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("connect failed: {e}"),
    };

    let too_large_secs = (i32::MAX as u64 / 1000) + 1;
    let err = match configure_client_socket(&stream, 30, too_large_secs) {
        Ok(()) => panic!("expected overflow validation error"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn test_normalize_ip() {
    // IPv4 stays IPv4
    let v4: SocketAddr = "192.168.1.1:8080".parse().unwrap();
    assert_eq!(normalize_ip(v4), v4);

    // Pure IPv6 stays IPv6
    let v6: SocketAddr = "[::1]:8080".parse().unwrap();
    assert_eq!(normalize_ip(v6), v6);
}

#[test]
fn test_listen_options_default() {
    let opts = ListenOptions::default();
    assert!(opts.reuse_addr);
    assert!(opts.reuse_port);
    assert_eq!(opts.backlog, 1024);
    assert_eq!(opts.client_mss, None);
}

#[cfg(target_os = "linux")]
#[test]
fn test_create_listener_applies_client_mss() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let options = ListenOptions {
        reuse_port: false,
        client_mss: Some(256),
        ..Default::default()
    };
    let socket = match create_listener(addr, &options) {
        Ok(socket) => socket,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("create_listener failed: {e}"),
    };
    let mss = match socket.tcp_mss() {
        Ok(mss) => mss,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("tcp_mss failed: {e}"),
    };
    assert_eq!(mss, 256);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_chunked_send_preserves_stream_and_configured_mss() {
    use std::os::fd::AsRawFd;

    let options = ListenOptions {
        reuse_port: false,
        client_mss: Some(1400),
        ..Default::default()
    };
    let socket = match create_listener("127.0.0.1:0".parse().unwrap(), &options) {
        Ok(socket) => socket,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return,
        Err(e) => panic!("create_listener failed: {e}"),
    };
    let listener = TcpListener::from_std(socket.into()).unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    let initial_response: Vec<u8> = (0..4096).map(|value| (value % 251) as u8).collect();
    let bulk_payload = vec![0xA5; 8192];
    let expected_len = initial_response.len() + bulk_payload.len();
    let reader = tokio::spawn(async move {
        let mut client = client;
        let mut received = vec![0; expected_len];
        client.read_exact(&mut received).await.unwrap();
        received
    });

    let mss_before = socket2::SockRef::from(&server).tcp_mss().unwrap();
    send_tcp_fragmented_fd(server.as_raw_fd(), &initial_response, 92)
        .await
        .unwrap();
    server.write_all(&bulk_payload).await.unwrap();
    let mss_after = socket2::SockRef::from(&server).tcp_mss().unwrap();

    let mut expected = initial_response;
    expected.extend_from_slice(&bulk_payload);
    assert_eq!(
        reader.await.unwrap(),
        expected,
        "chunked send must preserve the byte stream"
    );
    assert_eq!(
        mss_after, mss_before,
        "chunked send must not change the configured socket MSS"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_chunked_send_rejects_zero_fragment_size() {
    let error = send_tcp_fragmented_fd(-1, b"response", 0)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_chunked_send_rejects_invalid_fd() {
    let error = send_tcp_fragmented_fd(-1, b"response", 92)
        .await
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "manual descriptor-pressure gate"]
async fn test_chunked_send_has_no_fd_growth_after_success_and_cancellation_stress() {
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    let baseline_fds = std::fs::read_dir("/proc/self/fd").unwrap().count();

    let blocked_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let blocked_addr = blocked_listener.local_addr().unwrap();
    let _blocked_client = TcpStream::connect(blocked_addr).await.unwrap();
    let (blocked_server, _) = blocked_listener.accept().await.unwrap();
    socket2::SockRef::from(&blocked_server)
        .set_send_buffer_size(4 * 1024)
        .unwrap();
    let blocked_fd = blocked_server.as_raw_fd();
    let payload = Arc::new(vec![0xA5; 1024 * 1024]);
    for _ in 0..5_000 {
        let payload = payload.clone();
        let sender = tokio::spawn(async move {
            send_tcp_fragmented_fd(blocked_fd, payload.as_slice(), 92).await
        });
        tokio::task::yield_now().await;
        sender.abort();
        let _ = sender.await;
    }

    let success_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let success_addr = success_listener.local_addr().unwrap();
    let mut success_client = TcpStream::connect(success_addr).await.unwrap();
    let (success_server, _) = success_listener.accept().await.unwrap();
    let success_fd = success_server.as_raw_fd();
    let reader = tokio::spawn(async move {
        let mut received = vec![0_u8; 5_000];
        success_client.read_exact(&mut received).await.unwrap();
        received
    });
    for _ in 0..5_000 {
        send_tcp_fragmented_fd(success_fd, &[0x5A], 92)
            .await
            .unwrap();
    }
    assert!(reader.await.unwrap().iter().all(|byte| *byte == 0x5A));

    drop(success_server);
    drop(success_listener);
    drop(blocked_server);
    drop(_blocked_client);
    drop(blocked_listener);

    let final_fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
    assert_eq!(final_fds, baseline_fds);
}
