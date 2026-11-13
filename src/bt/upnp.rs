use anyhow::{bail, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const SSDP_MCAST: &str = "239.255.255.250:1900";
const RENEW_INTERVAL: Duration = Duration::from_secs(1800);
const LEASE_SECS: u32 = 3600;

pub struct UpnpGuard {
    shutdown: Arc<Notify>,
}

impl UpnpGuard {
    pub fn spawn(port: u16) -> Option<Self> {
        if port == 0 {
            return None;
        }
        let shutdown = Arc::new(Notify::new());
        let sd = shutdown.clone();
        tokio::spawn(async move {
            upnp_loop(port, sd).await;
        });
        Some(Self { shutdown })
    }
}

impl Drop for UpnpGuard {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}

async fn upnp_loop(port: u16, shutdown: Arc<Notify>) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    loop {
        if let Err(e) = try_map_once(&client, port).await {
            tracing::debug!("upnp port map attempt skipped/failed: {e}");
        }
        tokio::select! {
            _ = shutdown.notified() => return,
            _ = tokio::time::sleep(RENEW_INTERVAL) => {}
        }
    }
}

async fn try_map_once(client: &reqwest::Client, port: u16) -> Result<()> {
    let locations = tokio::task::spawn_blocking(discover_ssdp)
        .await
        .context("ssdp discover task")??;
    if locations.is_empty() {
        bail!("no ssdp gateway discovered");
    }

    for (loc, gateway_ip) in locations {
        let Ok(base) = url::Url::parse(&loc) else {
            continue;
        };
        let Ok(xml) = client.get(base.clone()).send().await else {
            continue;
        };
        let Ok(body) = xml.text().await else {
            continue;
        };
        let Some((service_type, control_rel)) = parse_control_service(&body) else {
            continue;
        };
        let Ok(control_url) = base.join(&control_rel) else {
            continue;
        };

        let internal_ip = match local_ip_for_gateway(gateway_ip) {
            Some(ip) => ip.to_string(),
            None => continue,
        };

        let mut mapped_any = false;
        for proto in ["TCP", "UDP"] {
            if send_add_port_mapping(
                client,
                control_url.as_str(),
                &service_type,
                port,
                proto,
                &internal_ip,
            )
            .await
            .is_ok()
            {
                mapped_any = true;
            }
        }
        if mapped_any {
            tracing::info!("upnp mapped port {port} TCP/UDP -> {internal_ip}");
            return Ok(());
        }
    }

    bail!("no upnp igd accepted AddPortMapping")
}

fn discover_ssdp() -> Result<Vec<(String, IpAddr)>> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("bind ssdp udp")?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(450)));

    let targets = [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
    ];
    for st in targets {
        let req = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_MCAST}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        );
        let _ = sock.send_to(req.as_bytes(), SSDP_MCAST);
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 4096];
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while std::time::Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(loc) = parse_ssdp_location(&buf[..n]) {
                    if let Ok(u) = url::Url::parse(&loc) {
                        // Ensure discovered HTTP descriptor points back to responder host/LAN IP.
                        let host_matches = match u.host() {
                            Some(url::Host::Ipv4(v4)) => IpAddr::V4(v4) == from.ip(),
                            Some(url::Host::Ipv6(v6)) => IpAddr::V6(v6) == from.ip(),
                            _ => false,
                        };
                        if host_matches && seen.insert(loc.clone()) {
                            out.push((loc, from.ip()));
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    Ok(out)
}

fn local_ip_for_gateway(gateway: IpAddr) -> Option<IpAddr> {
    let bind_addr = match gateway {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => return None,
    };
    let sock = UdpSocket::bind(bind_addr).ok()?;
    sock.connect(SocketAddr::new(gateway, 80)).ok()?;
    Some(sock.local_addr().ok()?.ip())
}

async fn send_add_port_mapping(
    client: &reqwest::Client,
    control_url: &str,
    service_type: &str,
    port: u16,
    protocol: &str,
    internal_ip: &str,
) -> Result<()> {
    let soap = format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:AddPortMapping xmlns:u="{service_type}">
      <NewRemoteHost></NewRemoteHost>
      <NewExternalPort>{port}</NewExternalPort>
      <NewProtocol>{protocol}</NewProtocol>
      <NewInternalPort>{port}</NewInternalPort>
      <NewInternalClient>{internal_ip}</NewInternalClient>
      <NewEnabled>1</NewEnabled>
      <NewPortMappingDescription>bitlab</NewPortMappingDescription>
      <NewLeaseDuration>{LEASE_SECS}</NewLeaseDuration>
    </u:AddPortMapping>
  </s:Body>
</s:Envelope>"#
    );
    let soap_action = format!("\"{service_type}#AddPortMapping\"");
    let resp = client
        .post(control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", soap_action)
        .body(soap)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("soap error status {}", resp.status());
    }
    Ok(())
}

pub fn parse_ssdp_location(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("LOCATION") {
                let loc = v.trim();
                if !loc.is_empty() {
                    return Some(loc.to_string());
                }
            }
        }
    }
    None
}

pub fn parse_control_service(xml: &str) -> Option<(String, String)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_service = false;
    let mut cur_tag: Option<String> = None;
    let mut stype: Option<String> = None;
    let mut ctrl: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_tag_name(e.name().as_ref());
                if name == "service" {
                    in_service = true;
                    stype = None;
                    ctrl = None;
                } else if in_service {
                    cur_tag = Some(name);
                }
            }
            Ok(Event::End(e)) => {
                let name = local_tag_name(e.name().as_ref());
                if name == "service" {
                    if let (Some(st), Some(cu)) = (stype.take(), ctrl.take()) {
                        if st.contains("WANIPConnection") || st.contains("WANPPPConnection") {
                            return Some((st, cu));
                        }
                    }
                    in_service = false;
                }
                cur_tag = None;
            }
            Ok(Event::Text(t)) => {
                if in_service {
                    if let (Some(tag), Ok(val)) = (cur_tag.as_deref(), t.unescape()) {
                        match tag {
                            "serviceType" => stype = Some(val.trim().to_string()),
                            "controlURL" => ctrl = Some(val.trim().to_string()),
                            _ => {}
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    None
}

fn local_tag_name(raw: &[u8]) -> String {
    let s = std::str::from_utf8(raw).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ssdp_location_and_xml_service() {
        let ssdp = b"HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLocation: http://192.168.1.1:5000/rootDesc.xml\r\n\r\n";
        assert_eq!(
            parse_ssdp_location(ssdp).as_deref(),
            Some("http://192.168.1.1:5000/rootDesc.xml")
        );

        let xml = r#"<root>
          <device>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
                <controlURL>/ctl/L3F</controlURL>
              </service>
              <service>
                <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
                <controlURL>/ctl/IPConn</controlURL>
              </service>
            </serviceList>
          </device>
        </root>"#;
        let parsed = parse_control_service(xml).unwrap();
        assert_eq!(parsed.0, "urn:schemas-upnp-org:service:WANIPConnection:1");
        assert_eq!(parsed.1, "/ctl/IPConn");
    }
}
