use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(2);

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_health_check_and_stops_on_signal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let empty = std::env::temp_dir().join(format!("koma-serve-empty-{}", std::process::id()));
    let state = koma::serve::AppState::new(&koma::cli::Paths {
        library: empty.join("library"),
        data: empty.join("data"),
    });
    let server = tokio::spawn(koma::serve::serve(listener, state, async {
        let _ = stopped.await;
    }));

    assert_eq!(
        blocking(move || koma::health::probe(addr, TIMEOUT)).await,
        Ok(())
    );
    // Anything but the health check, pages and assets needs an account first.
    assert_eq!(
        blocking(move || koma::health::status(addr, "/nothing-here", TIMEOUT)).await,
        Ok(303)
    );
    assert_eq!(
        blocking(move || koma::health::status(addr, "/a/nothing-here", TIMEOUT)).await,
        Ok(404)
    );
    // No account yet: every page leads to making the admin.
    assert_eq!(
        blocking(move || koma::health::status(addr, "/", TIMEOUT)).await,
        Ok(303)
    );

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[test]
fn health_check_fails_when_nothing_listens() {
    // Bind then drop, so the port is very likely closed.
    let addr: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert!(koma::health::probe(addr, TIMEOUT).is_err());
}
