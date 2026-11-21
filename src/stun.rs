use anyhow::{Context, Result, bail};
use std::{net::SocketAddr, time::Duration};
use str0m::Candidate;
use stun_types::{
    attribute::XorMappedAddress,
    message::{BINDING, Message, MessageClass, MessageWriteExt, MessageWriteVec, TransactionId},
};
use tokio::{
    net::{UdpSocket, lookup_host},
    sync::mpsc,
    time::timeout,
};

pub struct Binding {
    transaction: TransactionId,
    replies: mpsc::Sender<(SocketAddr, SocketAddr)>,
}

impl Binding {
    pub fn new() -> (
        Self,
        MessageWriteVec,
        mpsc::Receiver<(SocketAddr, SocketAddr)>,
    ) {
        let request = Message::builder_request(BINDING, MessageWriteVec::new());
        let (replies, rx) = mpsc::channel(4);
        (
            Self {
                transaction: request.transaction_id(),
                replies,
            },
            request,
            rx,
        )
    }

    // ICE and public STUN share one UDP mapping. Only consume our binding reply.
    pub fn receive(&self, source: SocketAddr, data: &[u8]) -> bool {
        if !data.starts_with(&[1, 1]) {
            return false;
        }
        let Ok(msg) = Message::from_bytes(data) else {
            return false;
        };
        if msg.transaction_id() != self.transaction
            || msg.method() != BINDING
            || msg.class() != MessageClass::Success
        {
            return false;
        }
        if let Ok(address) = msg.attribute::<XorMappedAddress>() {
            let _ = self
                .replies
                .try_send((source, address.addr(self.transaction)));
        }
        true
    }
}

pub async fn gather(
    socket: &UdpSocket,
    server: &str,
    request: MessageWriteVec,
    mut replies: mpsc::Receiver<(SocketAddr, SocketAddr)>,
) -> Result<Candidate> {
    let local = socket.local_addr()?;
    let server = timeout(Duration::from_secs(2), lookup_host(server))
        .await??
        .find(|addr| addr.is_ipv4() == local.is_ipv4())
        .context("No STUN server address for this interface")?;
    for millis in [500, 1000, 2000] {
        socket.send_to(&request, server).await?;
        let reply = async {
            while let Some((source, mapped)) = replies.recv().await {
                if source == server {
                    return Ok(Candidate::server_reflexive(mapped, local, "udp")?);
                }
            }
            bail!("Peer closed")
        };
        if let Ok(result) = timeout(Duration::from_millis(millis), reply).await {
            return result;
        }
    }
    bail!("STUN server did not respond")
}

#[tokio::test]
async fn gather_uses_only_the_expected_server_and_transaction() -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let server = UdpSocket::bind("127.0.0.1:0").await?;
    let source = server.local_addr()?;
    let address = source.to_string();
    let mapped: SocketAddr = "192.0.2.9:45678".parse()?;
    let (binding, request, rx) = Binding::new();
    let unrelated = Message::builder_request(BINDING, MessageWriteVec::new());
    let respond = async {
        let mut data = [0; 256];
        let (n, _) = server.recv_from(&mut data).await?;
        let reply = |buf: &[u8], addr| -> Result<_> {
            let msg = Message::from_bytes(buf)?;
            let mut response = Message::builder_success(&msg, MessageWriteVec::new());
            response.add_attribute(&XorMappedAddress::new(addr, msg.transaction_id()))?;
            Ok(response)
        };
        assert!(!binding.receive(source, &data[..n])); // ICE requests must still reach str0m.
        assert!(!binding.receive(source, &reply(&unrelated, mapped)?));
        assert!(binding.receive(socket.local_addr()?, &reply(&data[..n], source)?));
        assert!(binding.receive(source, &reply(&data[..n], mapped)?));
        anyhow::Ok(())
    };
    let (candidate, ()) = tokio::try_join!(gather(&socket, &address, request, rx), respond)?;
    assert_eq!(candidate.addr(), mapped);
    Ok(())
}
