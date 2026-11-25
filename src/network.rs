use anyhow::{Context, Result};
use netdev::prelude::InterfaceType;
use std::net::IpAddr;
use tokio::net::UdpSocket;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Interface {
    pub ip: IpAddr,
    index: Option<std::num::NonZeroU32>,
    pub wired: bool,
}

impl From<IpAddr> for Interface {
    fn from(ip: IpAddr) -> Self {
        Self {
            ip,
            index: None,
            wired: false,
        }
    }
}

impl Interface {
    pub async fn bind(&self) -> std::io::Result<UdpSocket> {
        let socket = UdpSocket::bind((self.ip, 0)).await?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self.index.is_some() {
            // Source binding alone does not select the NIC on a multihomed subnet.
            let sock = socket2::SockRef::from(&socket);
            if self.ip.is_ipv4() {
                sock.bind_device_by_index_v4(self.index)?;
            } else {
                sock.bind_device_by_index_v6(self.index)?;
            }
        }
        Ok(socket)
    }
}

pub fn interfaces() -> Result<Vec<Interface>> {
    let pinned = std::env::var("VOICE_BIND_IP")
        .ok()
        .map(|ip| ip.parse::<IpAddr>())
        .transpose()
        .context("Invalid VOICE_BIND_IP")?;
    let interfaces = netdev::get_interfaces();
    anyhow::ensure!(
        !interfaces.is_empty(),
        "Cannot enumerate network interfaces"
    );
    let mut found = Vec::new();
    for i in interfaces {
        if !i.is_up() || !i.is_running() || (i.is_loopback() && pinned.is_none()) {
            continue;
        }
        let wired = matches!(
            i.if_type,
            InterfaceType::Ethernet
                | InterfaceType::FastEthernetT
                | InterfaceType::FastEthernetFx
                | InterfaceType::GigabitEthernet
        ) && i.is_physical();
        for ip in i.ip_addrs() {
            if pinned.is_some_and(|p| p != ip)
                || ip.is_unspecified()
                || ip.is_multicast()
                || (pinned.is_none()
                    && match ip {
                        IpAddr::V4(a) => a.is_link_local(),
                        IpAddr::V6(a) => a.is_unicast_link_local(),
                    })
            {
                continue;
            }
            found.push(Interface {
                ip,
                index: std::num::NonZeroU32::new(i.index),
                wired,
            });
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}
