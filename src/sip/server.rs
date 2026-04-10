// Dosya: src/sip/server.rs
use crate::config::AppConfig;
use crate::rtp::engine::RtpEngine;
use crate::sip::engine::{SbcEngine, SipAction};
use anyhow::Result;
use dashmap::DashMap;
use sentiric_sip_core::{
    builder::SipResponseFactory, parser, HeaderName, Method, SipPacket, SipRouter, SipTransport,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub const DEFAULT_SIP_PORT: u16 = 5060;

pub struct DnsCache {
    cache: DashMap<String, (SocketAddr, Instant)>,
}

// [CLIPPY FIX]: new_without_default
impl Default for DnsCache {
    fn default() -> Self {
        Self::new()
    }
}

// [ARCH-COMPLIANCE] resolve fonksiyonu call_id argümanı alacak şekilde güncellendi.
impl DnsCache {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    pub async fn resolve(&self, hostname: &str, call_id: &str) -> Option<SocketAddr> {
        if let Ok(addr) = hostname.parse::<SocketAddr>() {
            return Some(addr);
        }

        let now = Instant::now();
        if let Some(cached) = self.cache.get(hostname) {
            let (addr, timestamp) = *cached;
            if now.duration_since(timestamp) < Duration::from_secs(60) {
                return Some(addr);
            }
        }

        let mut backoff = 100;
        for attempt in 1..=5 {
            match tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::lookup_host(hostname),
            )
            .await
            {
                Ok(Ok(mut addrs)) => {
                    if let Some(addr) = addrs.next() {
                        self.cache.insert(hostname.to_string(), (addr, now));
                        return Some(addr);
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(event="DNS_RETRY", sip.call_id=%call_id, attempt=attempt, host=%hostname, error=%e, "DNS çözümü gecikti, tekrar deneniyor...");
                }
                Err(_) => {
                    tracing::warn!(event="DNS_TIMEOUT", sip.call_id=%call_id, attempt=attempt, host=%hostname, "DNS çözümü 500ms timeout süresini aştı.");
                }
            }
            tokio::time::sleep(Duration::from_millis(backoff)).await;
            backoff *= 2;
        }

        if let Some(cached) = self.cache.get(hostname) {
            warn!(event="DNS_STALE_FALLBACK", sip.call_id=%call_id, host=%hostname, "⚠️ DNS ağdan çözümlenemedi (Discovery kapalı olabilir), önbellekteki eski IP adresi kullanılıyor.");
            return Some(cached.0);
        }

        error!(event="DNS_FATAL", sip.call_id=%call_id, host=%hostname, "❌ DNS çözümlenemedi ve önbellekte kayıt yok!");
        None
    }
}

pub struct SipServer {
    config: Arc<AppConfig>,
    transport: Arc<SipTransport>,
    engine: SbcEngine,
    dns_cache: Arc<DnsCache>,
}

impl SipServer {
    pub async fn new(config: Arc<AppConfig>) -> Result<Self> {
        let bind_addr = format!("{}:{}", config.sip_bind_ip, config.sip_port);
        let transport = SipTransport::new(&bind_addr).await?;
        let rtp_engine = Arc::new(RtpEngine::new(config.rtp_start_port, config.rtp_end_port));

        info!(
            event = "SIP_CONFIG_LOADED",
            sip.bind = %bind_addr,
            "SBC SIP Konfigürasyonu yüklendi (Lazy Routing Active)"
        );

        Ok(Self {
            config: config.clone(),
            transport: Arc::new(transport),
            engine: SbcEngine::new(config, rtp_engine),
            dns_cache: Arc::new(DnsCache::new()),
        })
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!(
            event = "SIP_SERVER_ACTIVE",
            bind_ip = %self.config.sip_bind_ip,
            port = self.config.sip_port,
            protocol = "UDP",
            "📡 SBC Sinyalleşme Sunucusu Aktif"
        );
        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, src_addr)) => {
                            if len < 4 { continue; }

                            if len <= 4 && buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                continue;
                            }

                            match parser::parse(&buf[..len]) {
                                Ok(packet) => {
                                    let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

                                    //[ARCH-COMPLIANCE] TYPE FIX: status_code is u16 natively.
                                    let method = if packet.is_request() {
                                        packet.method.as_str().to_string()
                                    } else {
                                        format!("RESPONSE/{}", packet.status_code)
                                    };

                                    if packet.is_request && packet.method == Method::Invite {
                                        let trying_packet = SipResponseFactory::create_100_trying(&packet);
                                        let trying_bytes = trying_packet.to_bytes();
                                        // [ARCH-COMPLIANCE] INFO yerine DEBUG yapıldı. Disk I/O tasarrufu!
                                        debug!(
                                            event = "SIP_EGRESS",
                                            sip.call_id = %call_id,
                                            sip.method = "100",
                                            net.dst.ip = %src_addr.ip(),
                                            net.dst.port = src_addr.port(),
                                            packet.summary = "SIP/2.0 100 Trying",
                                            "📤[SBC->UAC] 100 Trying gönderildi"
                                        );

                                        let _ = self.transport.send(&trying_bytes, src_addr).await;
                                    }

                                    // [ARCH-COMPLIANCE] SUTS v4.2: Her paket alımı detayı DEBUG'a çekildi.
                                    debug!(
                                        event = "SIP_PACKET_RECEIVED",
                                        sip.call_id = %call_id,
                                        sip.method = %method,
                                        net.src.ip = %src_addr.ip(),
                                        net.src.port = src_addr.port(),
                                        "📥 SIP paketi alındı"
                                    );

                                    if let SipAction::Forward(mut processed) = self.engine.inspect(packet, src_addr).await {
                                        self.route_packet(&mut processed, src_addr).await;
                                    }
                                },
                                Err(e) => warn!(
                                    event = "SIP_PARSE_ERROR",
                                    net.peer.ip = %src_addr.ip(),
                                    error = %e,
                                    "⚠️ Bozuk veya geçersiz SIP paketi alındı"
                                ),
                            }
                        },
                        Err(e) => error!(event="SIP_SOCKET_ERROR", error=%e, "🔥 UDP Socket okuma hatası"),
                    }
                }
            }
        }
    }

    async fn route_packet(&self, packet: &mut SipPacket, src_addr: SocketAddr) {
        let call_id = packet
            .get_header_value(HeaderName::CallId)
            .cloned()
            .unwrap_or_default();
        // [ARCH-COMPLIANCE] resolve işlemine call_id aktarıldı
        let proxy_addr_opt = self
            .dns_cache
            .resolve(&self.config.proxy_sip_addr, &call_id)
            .await;

        let target_addr = if packet.is_request() {
            if let Some(proxy_addr) = proxy_addr_opt {
                if src_addr.ip() == proxy_addr.ip() {
                    sentiric_sip_core::utils::extract_socket_addr(&packet.uri)
                } else {
                    Some(proxy_addr)
                }
            } else {
                None
            }
        } else {
            packet
                .get_header_value(HeaderName::Via)
                .and_then(|v| SipRouter::resolve_response_target(v, DEFAULT_SIP_PORT))
        };

        if let Some(target) = target_addr {
            let packet_bytes = packet.to_bytes();
            let debug_line = String::from_utf8_lossy(&packet_bytes[..packet_bytes.len().min(50)]);
            let full_payload = String::from_utf8_lossy(&packet_bytes).to_string();

            let call_id = packet
                .get_header_value(HeaderName::CallId)
                .cloned()
                .unwrap_or_default();

            // [ARCH-COMPLIANCE] TYPE FIX
            let method = if packet.is_request() {
                packet.method.as_str().to_string()
            } else {
                format!("RESPONSE/{}", packet.status_code)
            };

            // [ARCH-COMPLIANCE] INFO yerine DEBUG yapıldı. Disk I/O tasarrufu!
            debug!(
                event = "SIP_EGRESS_FULL",
                sip.call_id = %call_id,
                sip.method = %method,
                net.dst.ip = %target.ip(),
                net.dst.port = target.port(),
                packet.summary = %debug_line.trim_end(),
                payload = %full_payload,
                "📤[SBC->NEXT] Paket yönlendiriliyor (Tam Döküm)"
            );

            if let Err(e) = self.transport.send(&packet_bytes, target).await {
                error!(
                    event = "SIP_SEND_ERROR",
                    sip.call_id = %call_id,
                    net.dst.ip = %target.ip(),
                    error = %e,
                    "🔥 SIP paketi hedefe gönderilemedi"
                );
            }
        } else {
            let call_id = packet
                .get_header_value(HeaderName::CallId)
                .cloned()
                .unwrap_or_default();
            error!(
                event = "SIP_ROUTE_FAIL",
                sip.call_id = %call_id,
                "❌ Yanıt veya Dışarı giden paket için hedef adres çözümlenemedi. Paket düşürüldü."
            );
        }
    }
}
