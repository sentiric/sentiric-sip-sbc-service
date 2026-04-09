// Dosya: sentiric-sip-sbc-service/src/rtp/engine.rs
use dashmap::DashMap;
use rand::Rng;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{error, info, warn};

fn is_internal_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            if octets[0] == 10 && octets[1] == 88 && octets[3] == 1 {
                return true;
            }
            if octets[0] == 10 || octets[0] == 127 {
                return true;
            }
            if octets[0] == 172 && (octets[1] >= 16 && octets[1] <= 31) {
                return true;
            }
            if octets[0] == 192 && octets[1] == 168 {
                return true;
            }
            if octets[0] == 100 && (octets[1] >= 64 && octets[1] <= 127) {
                return true;
            }
            false
        }
        IpAddr::V6(ipv6) => ipv6.is_loopback(),
    }
}

fn is_docker_gateway(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[3] == 1,
        _ => false,
    }
}

struct RtpRelay {
    stop_signal: tokio::sync::broadcast::Sender<()>,
}

pub struct RtpEngine {
    active_relays: Arc<DashMap<u16, RtpRelay>>,
    call_id_map: Arc<DashMap<String, u16>>,
    start_port: u16,
    end_port: u16,
}

impl RtpEngine {
    pub fn new(start: u16, end: u16) -> Self {
        Self {
            active_relays: Arc::new(DashMap::new()),
            call_id_map: Arc::new(DashMap::new()),
            start_port: start,
            end_port: end,
        }
    }

    pub async fn get_or_allocate_relay(
        &self,
        call_id: &str,
        initial_peer: Option<SocketAddr>,
    ) -> Option<u16> {
        if let Some(entry) = self.call_id_map.get(call_id) {
            return Some(*entry.value());
        }

        let mut rng = rand::thread_rng();
        for _ in 0..1000 {
            let port = rng.gen_range(self.start_port..=self.end_port);
            let port = if port % 2 != 0 {
                port.saturating_add(1)
            } else {
                port
            };
            if port > self.end_port {
                continue;
            }

            if !self.active_relays.contains_key(&port) {
                let (tx, _) = tokio::sync::broadcast::channel(1);
                let relay = RtpRelay {
                    stop_signal: tx.clone(),
                };

                let active_relays_clone = self.active_relays.clone();
                let call_id_map_clone = self.call_id_map.clone();
                let call_id_owned = call_id.to_string();
                let stop_rx = tx.subscribe();

                tokio::spawn(async move {
                    info!(
                        event = "RTP_RELAY_STARTED",
                        sip.call_id = %call_id_owned,
                        rtp.port = port,
                        "🚀[RTP-RELAY] Başlatıldı"
                    );

                    if let Err(e) =
                        run_relay_loop(port, stop_rx, initial_peer, &call_id_owned).await
                    {
                        error!(
                            event = "RTP_RELAY_ERROR",
                            sip.call_id = %call_id_owned,
                            rtp.port = port,
                            error = %e,
                            "🔥[RTP-RELAY] Hata oluştu"
                        );
                    }
                    active_relays_clone.remove(&port);
                    call_id_map_clone.remove(&call_id_owned);
                });

                self.active_relays.insert(port, relay);
                self.call_id_map.insert(call_id.to_string(), port);
                return Some(port);
            }
        }

        warn!(
            event = "RTP_PORT_EXHAUSTED",
            trace_id = %call_id,
            sip.call_id = %call_id,
            "Port aralığı tükendi, relay ayrılamıyor."
        );
        None
    }

    pub async fn release_relay_by_call_id(&self, call_id: &str) -> bool {
        if let Some((_, port)) = self.call_id_map.remove(call_id) {
            if let Some((_, relay)) = self.active_relays.remove(&port) {
                let _ = relay.stop_signal.send(());

                info!(
                    event = "RTP_RELAY_RELEASED",
                    trace_id = %call_id,
                    sip.call_id = %call_id,
                    rtp.port = port,
                    "🛑 RTP Relay serbest bırakıldı."
                );
                return true;
            }
        }
        false
    }
}

async fn run_relay_loop(
    port: u16,
    mut stop_signal: tokio::sync::broadcast::Receiver<()>,
    initial_peer: Option<SocketAddr>,
    call_id: &str,
) -> anyhow::Result<()> {
    let addr = format!("0.0.0.0:{}", port);
    let socket = UdpSocket::bind(&addr).await?;
    let mut buf = [0u8; 2048];

    let mut peer_external: Option<SocketAddr> = None;
    let mut external_latched = false;

    let mut peer_internal: Option<SocketAddr> = None;
    let mut internal_latched = false;

    let timeout = Duration::from_secs(60);

    tracing::debug!(
        event = "RTP_SOCKET_BOUND",
        sip.call_id = %call_id,
        rtp.port = port,
        "🎧 RTP Relay soketi IP adresine bağlandı ve dinliyor."
    );

    if let Some(target) = initial_peer {
        if is_internal_ip(target.ip()) {
            tracing::info!(
                event="RTP_PRE_LATCH",
                sip.call_id = %call_id,
                target=%target,
                "🏢 İç Hedef tespit edildi. Sinyal bekleniyor."
            );
            peer_internal = Some(target);
        } else {
            tracing::info!(
                event="RTP_PRE_LATCH",
                sip.call_id = %call_id,
                target=%target,
                "🌍 Dış Hedef tespit edildi. Sinyal bekleniyor."
            );
            peer_external = Some(target);
        }
    }

    loop {
        tokio::select! {
            _ = stop_signal.recv() => break,
            res = tokio::time::timeout(timeout, socket.recv_from(&mut buf)) => {
                match res {
                    Ok(Ok((len, src))) => {
                        let is_internal = is_internal_ip(src.ip());

                        // [ARCH-COMPLIANCE FIX]: RTCP paketlerini (Payload Type 192-205) tespit et.
                        let is_rtcp = len >= 2 && (buf[0] >> 6 == 2) && (buf[1] >= 192 && buf[1] <= 205);

                        if is_internal {
                            let is_docker_gw = is_docker_gateway(src.ip());

                            // [CRITICAL FIX]: Latching Logic Revised
                            let should_latch = match peer_internal {
                                None => !is_docker_gw && !is_rtcp,
                                Some(curr) => {
                                    if !internal_latched && !is_docker_gw && !is_rtcp {
                                        // Henüz kesin kilitlenmediyse (Sadece PRE-LATCH varsa), ilk gelen geçerli RTP paketiyle KİLİTLEN.
                                        true
                                    } else if curr.ip() != src.ip() && !is_docker_gw && !is_rtcp {
                                        // Kesin kilitlense BİLE IP adresi tamamen değiştiyse (Network Handover) YENİDEN KİLİTLEN.
                                        true
                                    } else {
                                        // Kesin kilitli ve IP aynı. Sadece port değişmişse (RTCP olabilir), KİLİDİ BOZMA.
                                        false
                                    }
                                }
                            };

                            if should_latch {
                                tracing::info!(
                                    event = "RTP_LATCH_INTERNAL",
                                    trace_id = %call_id,
                                    sip.call_id = %call_id,
                                    rtp.port = port,
                                    net.peer.ip = %src.ip(),
                                    net.peer.port = src.port(),
                                    "🏢[LATCH-INT] İç Bacak Kesin Olarak Kilitlendi (Strict Latch)"
                                );
                                peer_internal = Some(src);
                                internal_latched = true;
                            }

                            if let Some(dst) = peer_external {
                                if let Err(e) = socket.send_to(&buf[..len], dst).await {
                                    tracing::warn!(
                                        event = "RTP_UDP_SEND_ERROR",
                                        sip.call_id = %call_id,
                                        rtp.port = port,
                                        net.dst.ip = %dst.ip(),
                                        error = %e,
                                        "RTP paketi hedefe (Dış) gönderilemedi."
                                    );
                                }
                            }

                        } else {
                            // [CRITICAL FIX]: Latching Logic Revised (External)
                            let should_latch = match peer_external {
                                None => !is_rtcp,
                                Some(curr) => {
                                    if !external_latched && !is_rtcp {
                                        // SDP'den gelen tahmini porta (PRE-LATCH) güvenme. Dışarıdan gelen İLK gerçek RTP (ses) pakediyle portu ez ve KİLİTLEN.
                                        true
                                    } else if curr.ip() != src.ip() && !is_rtcp {
                                        // Mobil istemci Wi-Fi'dan 4G'ye geçti. IP değişti. Yeni IP'ye KİLİTLEN.
                                        true
                                    } else {
                                        // Kesin kilitlendik ve IP aynı. Sadece port değiştiyse (Örn: RTCP pakedi geldi) KİLİDİ BOZMA.
                                        false
                                    }
                                }
                            };

                            if should_latch {
                                tracing::info!(
                                    event = "RTP_LATCH_EXTERNAL",
                                    trace_id = %call_id,
                                    sip.call_id = %call_id,
                                    rtp.port = port,
                                    net.peer.ip = %src.ip(),
                                    net.peer.port = src.port(),
                                    "🌍[LATCH-EXT] Dış Bacak Kesin Olarak Kilitlendi! (SES GELİYOR)"
                                );
                                peer_external = Some(src);
                                external_latched = true;
                            }

                            // [CRITICAL FIX]: Gelen paket RTCP olsa bile içeriye (Media Service'e) yollamaya devam et.
                            // Latching yapmamak (kilitlenmemek) paketi çöpe atmak anlamına gelmez!
                            if let Some(dst) = peer_internal {
                                if let Err(e) = socket.send_to(&buf[..len], dst).await {
                                    tracing::warn!(
                                        event = "RTP_UDP_SEND_ERROR",
                                        sip.call_id = %call_id,
                                        rtp.port = port,
                                        net.dst.ip = %dst.ip(),
                                        error = %e,
                                        "RTP paketi hedefe (İç) gönderilemedi."
                                    );
                                }
                            }
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {
                        tracing::warn!(
                            event = "RTP_RELAY_TIMEOUT",
                            trace_id = %call_id,
                            sip.call_id = %call_id,
                            rtp.port = port,
                            "⌛ RTP Relay zaman aşımına uğradı."
                        );
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
