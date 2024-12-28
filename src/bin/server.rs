use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::{future, pin_mut, stream::TryStreamExt, SinkExt, StreamExt};
use native_tls::{Identity, TlsAcceptor};
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_native_tls::TlsAcceptor as TokioTlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

type Tx = UnboundedSender<Message>;
type PeerMap = Arc<Mutex<HashMap<SocketAddr, Tx>>>;

struct ChatServer {
    peer_map: PeerMap,
    addr: String,
    tls_acceptor: Option<TlsAcceptor>,
}

impl ChatServer {
    pub fn new(addr: String) -> Self {
        ChatServer {
            peer_map: Arc::new(Mutex::new(HashMap::new())),
            addr,
            tls_acceptor: None,
        }
    }

    pub fn with_tls_config(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Create certificate parameters
        let mut params = CertificateParams::new(vec!["localhost".to_string()]);

        // Set distinguished name
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "localhost");
        dn.push(DnType::OrganizationName, "Chatroom Server");
        dn.push(DnType::CountryName, "CN");
        params.distinguished_name = dn;

        // Generate certificate
        let cert = Certificate::from_params(params)?;
        let cert_pem = cert.serialize_pem()?;
        let key_pem = cert.serialize_private_key_pem();

        // Save certificate and key to files
        fs::write("cert.pem", &cert_pem)?;
        fs::write("key.pem", &key_pem)?;

        // Create PKCS#12 identity
        let identity = Identity::from_pkcs8(cert_pem.as_bytes(), key_pem.as_bytes())?;

        // Create TLS acceptor
        self.tls_acceptor = Some(TlsAcceptor::new(identity)?);
        Ok(())
    }

    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(&self.addr).await?;
        println!("WebSocket server listening on: {}", self.addr);

        while let Ok((stream, addr)) = listener.accept().await {
            let peer_map = self.peer_map.clone();
            let tls_acceptor = self
                .tls_acceptor
                .as_ref()
                .map(|acceptor| TokioTlsAcceptor::from(acceptor.clone()));

            tokio::spawn(async move {
                if let Some(acceptor) = tls_acceptor {
                    match acceptor.accept(stream).await {
                        Ok(tls_stream) => match tokio_tungstenite::accept_async(tls_stream).await {
                            Ok(ws_stream) => {
                                Self::handle_connection(peer_map, ws_stream, addr).await;
                            }
                            Err(e) => println!("Error during WebSocket handshake: {}", e),
                        },
                        Err(e) => println!("Error during TLS handshake: {}", e),
                    }
                } else {
                    match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws_stream) => {
                            Self::handle_connection(peer_map, ws_stream, addr).await;
                        }
                        Err(e) => println!("Error during WebSocket handshake: {}", e),
                    }
                }
            });
        }

        Ok(())
    }

    async fn handle_connection<S>(peer_map: PeerMap, ws_stream: S, addr: SocketAddr)
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + SinkExt<Message>
            + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
    {
        println!("Incoming TCP connection from: {}", addr);

        let (write, read) = ws_stream.split();
        let (tx, rx) = unbounded();
        peer_map.lock().await.insert(addr, tx);

        let broadcast_incoming = Self::handle_incoming_messages(peer_map.clone(), addr, read);
        let receive_from_others = rx.map(Ok).forward(write);

        pin_mut!(broadcast_incoming, receive_from_others);
        match future::select(broadcast_incoming, receive_from_others).await {
            future::Either::Left((result, _)) => {
                if let Err(e) = result {
                    println!("Error in broadcast_incoming for {}: {}", addr, e);
                }
            }
            future::Either::Right((result, _)) => {
                if let Err(e) = result {
                    println!("Error in receive_from_others for {}: {}", addr, e);
                }
            }
        }

        println!("{} disconnected", &addr);
        peer_map.lock().await.remove(&addr);
    }

    async fn handle_incoming_messages(
        peer_map: PeerMap,
        addr: SocketAddr,
        incoming: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    ) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        incoming
            .try_for_each(move |msg| {
                let peer_map = peer_map.clone();
                async move {
                    if let Ok(text) = msg.to_text() {
                        println!("Received message from {}: {}", addr, text);
                        let peers = peer_map.lock().await;

                        let broadcast_recipients: Vec<_> = peers
                            .iter()
                            .filter(|(peer_addr, _)| peer_addr != &&addr)
                            .map(|(_, ws_sink)| ws_sink)
                            .collect();

                        for recp in broadcast_recipients {
                            if let Err(e) = recp.unbounded_send(msg.clone()) {
                                println!("Error broadcasting message: {}", e);
                            }
                        }
                    }
                    Ok(())
                }
            })
            .await
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let mut server = ChatServer::new("127.0.0.1:8080".to_string());

    // Configure TLS
    if let Err(e) = server.with_tls_config() {
        eprintln!("Error configuring TLS: {}", e);
        return;
    }

    if let Err(e) = server.run().await {
        eprintln!("Error running server: {}", e);
    }
}
