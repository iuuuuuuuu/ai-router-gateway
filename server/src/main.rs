use gw_server::{admin, config, health, proxy, routes, store, tls};

use std::net::SocketAddr;
use std::sync::Arc;

fn default_db_path() -> std::path::PathBuf {
    // GW_DATA_DIR: a directory (gateway.db joined inside) or an explicit db file
    if let Some(v) = std::env::var("GW_DATA_DIR").ok().filter(|s| !s.is_empty()) {
        let p = std::path::PathBuf::from(&v);
        if p.is_dir() || v.ends_with('/') || v.ends_with('\\') {
            return p.join("gateway.db");
        }
        return p;
    }
    let mut p = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    p.pop();
    p.join("gateway.db")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let db_path = default_db_path();
    let store = Arc::new(store::Store::open(db_path.to_str().expect("db path utf8"))?);
    tracing::info!("sqlite opened at {}", db_path.display());

    // load persisted config (or default empty)
    let config = store.load_config().unwrap_or_default();
    let config = config.unwrap_or_default();

    let health = Arc::new(health::HealthRegistry::new(Some(store.clone())));
    let holder = config::ConfigHolder::default();
    holder.publish(config);

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

    let state = Arc::new(proxy::AppState {
        http: http_client,
        config: holder,
        health: health.clone(),
        store: store.clone(),
    });

    // data plane (HTTP + HTTPS, same router)
    let data = routes::data_plane_router((*state).clone());
    let snap = state.config.get();
    let (http_addr, https_addr, https_enabled) = snap
        .as_ref()
        .map(|s| {
            let l = &s.config.listen;
            (
                l.http.clone(),
                l.https.clone(),
                l.https_enabled,
            )
        })
        .unwrap_or_else(|| ("0.0.0.0:8080".into(), "0.0.0.0:8443".into(), true));

    let (cert, key) = tls::ensure_self_signed().expect("self-signed cert");
    let tls_cfg = axum_server::tls_rustls::RustlsConfig::from_der(vec![cert], key).await?;

    let data_http = data.clone();
    let lhttp = tokio::spawn(async move {
        let addr: SocketAddr = http_addr.parse().expect("http addr");
        let l = tokio::net::TcpListener::bind(addr).await.expect("bind http");
        tracing::info!("data plane http on http://{addr}");
        axum::serve(l, data_http).await.unwrap();
    });

    let data_https = data.clone();
    let lhttps = tokio::spawn(async move {
        if !https_enabled {
            tracing::info!("https disabled by config");
            return;
        }
        let addr: SocketAddr = https_addr.parse().expect("https addr");
        tracing::info!("data plane https on https://{addr}");
        axum_server::bind_rustls(addr, tls_cfg)
            .serve(data_https.into_make_service())
            .await
            .unwrap();
    });

    // admin plane (HTTP only, localhost by default)
    let admin_state = state.clone();
    let admin_bind: SocketAddr = snap
        .as_ref()
        .map(|s| s.config.admin.bind.clone())
        .unwrap_or_else(|| "127.0.0.1:9090".into())
        .parse()
        .expect("admin addr");
    let ladmin = tokio::spawn(async move {
        let app = admin::admin_router((*admin_state).clone());
        let l = tokio::net::TcpListener::bind(admin_bind).await.expect("bind admin");
        tracing::info!("admin plane on http://{admin_bind}");
        axum::serve(l, app).await.unwrap();
    });

    let _ = tokio::join!(lhttp, lhttps, ladmin);
    Ok(())
}
