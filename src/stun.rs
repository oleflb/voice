use anyhow::{Context, Result, bail};
use std::{net::SocketAddr, time::Duration};
use str0m::Candidate;
use stun_types::{
    attribute::XorMappedAddress,
    message::{BINDING, Message, MessageClass, MessageWriteExt, MessageWriteVec, TransactionId},
};
use tokio::{
    net::{UdpSocket, lookup_host},
    sync::{mpsc, watch},
    time::timeout,
};

pub struct Binding {
    transaction: TransactionId,
    replies: mpsc::Sender<(SocketAddr, SocketAddr)>,
}

impl Binding {
    fn new() -> (
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

pub async fn discover(
    socket: &UdpSocket,
    server: &str,
    active: watch::Sender<Option<Binding>>,
    mut failed: impl FnMut(anyhow::Error),
) -> Option<Candidate> {
    tokio::select! {
        biased;
        _ = active.closed() => None,
        candidate = async {
            let mut delay = Duration::from_secs(1);
            loop {
                let (binding, request, replies) = Binding::new();
                active.send(Some(binding)).ok()?;
                let result = gather(socket, server, request, replies).await;
                active.send(None).ok()?;
                match result {
                    Ok(candidate) => return Some(candidate),
                    Err(e) => failed(e),
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        } => candidate,
    }
}

async fn gather(
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
async fn discovery_retries_until_route_is_dropped() -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let (active, route) = watch::channel(None);
    let (failed, mut failures) = mpsc::unbounded_channel();
    let job = tokio::spawn(async move {
        discover(&socket, "[::1]:3478", active, |_| {
            failed.send(tokio::time::Instant::now()).unwrap();
        })
        .await
    });
    let first = timeout(Duration::from_secs(5), failures.recv())
        .await?
        .unwrap();
    let second = timeout(Duration::from_secs(5), failures.recv())
        .await?
        .unwrap();
    assert!(second - first >= Duration::from_secs(1));
    drop(route);
    assert!(timeout(Duration::from_millis(500), job).await??.is_none());
    Ok(())
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
