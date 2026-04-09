// src/app.rs
use crate::config::AppConfig;
use crate::grpc::service::MySbcService;
use crate::sip::server::SipServer;
use crate::telemetry::SutsFormatter;
use crate::tls::load_server_tls_config;
use anyhow::{Context, Result};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server as HttpServer, StatusCode,
};
use sentiric_contracts::sentiric::sip::v1::sbc_service_server::SbcServiceServer;
use std::convert::Infallible;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc;
use tonic::transport::Server as GrpcServer;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};

pub struct App {
    config: Arc<AppConfig>,
}

async fn handle_http_request(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"status":"ok", "service": "sip-sbc-service"}"#,
        ))
        .unwrap())
}

impl App {
    pub async fn bootstrap() -> Result<Self> {
        dotenvy::dotenv().ok();
        let config =
            Arc::new(AppConfig::load_from_env().context("Konfigürasyon dosyası yüklenemedi")?);

        let rust_log_env = env::var("RUST_LOG").unwrap_or_else(|_| config.rust_log.clone());
        let env_filter =
            EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&rust_log_env))?;
        let subscriber = Registry::default().with(env_filter);

        if config.log_format == "json" {
            // [ARCH-COMPLIANCE] Dinamik tenant_id formatöre iletiliyor
            let suts_formatter = SutsFormatter::new(
                "sip-sbc-service".to_string(),
                config.service_version.clone(),
                config.env.clone(),
                config.node_hostname.clone(),
                config.tenant_id.clone(),
            );
            subscriber
                .with(fmt::layer().event_format(suts_formatter))
                .init();
        } else {
            subscriber.with(fmt::layer().compact()).init();
        }

        info!(
            event = "SYSTEM_STARTUP",
            service_name = "sip-sbc-service",
            version = %config.service_version,
            profile = %config.env,
            "🚀 Servis başlatılıyor (SUTS v4.0 Active)..."
        );

        Ok(Self { config })
    }

    pub async fn run(self) -> Result<()> {
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let (sip_shutdown_tx, sip_shutdown_rx) = mpsc::channel(1);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();

        let sip_config = self.config.clone();
        let sip_server = SipServer::new(sip_config).await?;
        let sip_handle = tokio::spawn(async move {
            sip_server.run(sip_shutdown_rx).await;
        });

        let grpc_config = self.config.clone();
        let grpc_server_handle = tokio::spawn(async move {
            let tls_config = load_server_tls_config(&grpc_config)
                .await
                .expect("TLS yapılandırması başarısız");
            let grpc_service = MySbcService {};

            info!(
                event = "GRPC_SERVER_START",
                address = %grpc_config.grpc_listen_addr,
                "Güvenli gRPC sunucusu dinlemeye başlıyor..."
            );

            GrpcServer::builder()
                .tls_config(tls_config)
                .expect("TLS yapılandırma hatası")
                .add_service(SbcServiceServer::new(grpc_service))
                .serve_with_shutdown(grpc_config.grpc_listen_addr, async {
                    shutdown_rx.recv().await;
                    info!(
                        event = "GRPC_SHUTDOWN_SIGNAL",
                        "gRPC sunucusu için kapatma sinyali alındı."
                    );
                })
                .await
                .context("gRPC sunucusu hatayla sonlandı")
        });

        let http_config = self.config.clone();
        let http_server_handle = tokio::spawn(async move {
            let addr = http_config.http_listen_addr;
            let make_svc = make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(handle_http_request))
            });

            let server = HttpServer::bind(&addr)
                .serve(make_svc)
                .with_graceful_shutdown(async {
                    http_shutdown_rx.await.ok();
                });

            info!(
                event = "HTTP_SERVER_START",
                address = %addr,
                "HTTP sağlık kontrol sunucusu dinlemeye başlıyor..."
            );

            if let Err(e) = server.await {
                error!(event = "HTTP_SERVER_ERROR", error = %e, "HTTP sunucusu hatayla sonlandı");
            }
        });

        let ctrl_c = async {
            tokio::signal::ctrl_c()
                .await
                .expect("Ctrl+C dinleyicisi kurulamadı");
        };

        // [ARCH-COMPLIANCE] Çöken süreçler sessizce geçilmemeli, SUTS kuralına uygun şekilde raporlanmalıdır.
        tokio::select! {
            res = grpc_server_handle => {
                let inner_res = res.context("gRPC sunucu görevi panic'ledi")?;
                inner_res?;
                error!(event = "UNEXPECTED_SHUTDOWN", "gRPC sunucusu beklenmedik şekilde sonlandı!");
            },
            res = http_server_handle => {
                error!(event = "HTTP_SERVER_CRASH", result = ?res, "HTTP sunucu görevi çöktü.");
            },
            res = sip_handle => {
                error!(event = "SIP_SERVER_CRASH", result = ?res, "SIP sunucu görevi çöktü.");
            },
            _ = ctrl_c => {
                warn!(event = "SIGINT_RECEIVED", "Kapatma sinyali (Ctrl+C) alındı.");
            },
        }

        warn!(
            event = "SYSTEM_SHUTDOWN",
            "Graceful shutdown başlatılıyor..."
        );
        let _ = shutdown_tx.send(()).await;
        let _ = sip_shutdown_tx.send(()).await;
        let _ = http_shutdown_tx.send(());

        info!(event = "SYSTEM_STOPPED", "Servis başarıyla durduruldu.");
        Ok(())
    }
}
