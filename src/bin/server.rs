use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::{future, pin_mut, SinkExt, StreamExt};
use native_tls::{Identity, TlsAcceptor};
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_native_tls::TlsAcceptor as TokioTlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS error: {0}")]
    Tls(#[from] native_tls::Error),
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
}

type Result<T> = std::result::Result<T, ServerError>;
type Tx = UnboundedSender<Message>;
type PeerMap = Arc<Mutex<HashMap<SocketAddr, Tx>>>;
type ClientIdMap = Arc<Mutex<HashMap<SocketAddr, String>>>;

enum Stream {
    Plain(WebSocketStream<TcpStream>),
    Tls(WebSocketStream<tokio_native_tls::TlsStream<TcpStream>>),
}

impl Stream {
    async fn handle_connection(self, handler: &ConnectionHandler, addr: SocketAddr) -> Result<()> {
        match self {
            Stream::Plain(ws_stream) => handler.handle_connection(ws_stream, addr).await,
            Stream::Tls(ws_stream) => handler.handle_connection(ws_stream, addr).await,
        }
    }
}

struct TlsConfig {
    acceptor: TlsAcceptor,
}

impl TlsConfig {
    pub fn new() -> Result<Self> {
        let mut params = CertificateParams::new(vec!["localhost".to_string()]);
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "localhost");
        dn.push(DnType::OrganizationName, "Chatroom Server");
        dn.push(DnType::CountryName, "CN");
        params.distinguished_name = dn;

        let cert = Certificate::from_params(params).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let cert_pem = cert
            .serialize_pem()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let key_pem = cert.serialize_private_key_pem();

        fs::write("cert.pem", &cert_pem)?;
        fs::write("key.pem", &key_pem)?;

        let identity = Identity::from_pkcs8(cert_pem.as_bytes(), key_pem.as_bytes())?;
        Ok(TlsConfig {
            acceptor: TlsAcceptor::new(identity)?,
        })
    }

    pub fn get_acceptor(&self) -> TokioTlsAcceptor {
        TokioTlsAcceptor::from(self.acceptor.clone())
    }
}

#[derive(Clone)]
struct ConnectionHandler {
    peer_map: PeerMap,
    client_ids: ClientIdMap,
}

impl ConnectionHandler {
    pub fn new(peer_map: PeerMap) -> Self {
        ConnectionHandler {
            peer_map,
            client_ids: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn handle_connection<S>(&self, ws_stream: WebSocketStream<S>, addr: SocketAddr) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        WebSocketStream<S>:
            StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + SinkExt<Message>,
    {
        println!("New WebSocket connection: {}", addr);
        let client_id = Uuid::new_v4().to_string();
        let short_id = &client_id[0..8]; // 只使用前8位
        self.client_ids.lock().await.insert(addr, client_id.clone());
        let (tx, rx) = unbounded();
        self.peer_map.lock().await.insert(addr, tx.clone());

        let welcome_msg = Message::Text(format!("Welcome! Your ID: {}", short_id));
        if let Err(e) = tx.unbounded_send(welcome_msg) {
            println!("Failed to send welcome message to {}: {}", addr, e);
        }

        let (outgoing, incoming) = ws_stream.split();
        let broadcast_incoming = self.handle_incoming_messages(addr, incoming);
        let receive_from_others = rx.map(Ok).forward(outgoing);

        pin_mut!(broadcast_incoming, receive_from_others);
        future::select(broadcast_incoming, receive_from_others).await;

        println!("{} disconnected", addr);
        self.peer_map.lock().await.remove(&addr);
        self.client_ids.lock().await.remove(&addr);
        Ok(())
    }

    async fn handle_incoming_messages<S>(&self, addr: SocketAddr, mut incoming: S) -> Result<()>
    where
        S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        let peer_map = self.peer_map.clone();
        let client_ids = self.client_ids.clone();

        while let Some(msg) = incoming.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let sender_id = match client_ids.lock().await.get(&addr) {
                        Some(id) => id[0..8].to_string(), // 使用简短的ID
                        None => continue,
                    };
                    println!("Received from {}: {}", sender_id, text); // 显示来源ID和消息内容
                    let broadcast_msg = Message::Text(format!("{}: {}", sender_id, text));

                    let peers = peer_map.lock().await;
                    for peer in peers.iter() {
                        if *peer.0 != addr {
                            if let Err(e) = peer.1.unbounded_send(broadcast_msg.clone()) {
                                println!("Failed to send message to {}: {}", peer.0, e);
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    println!("Client {} disconnected.", addr);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }
}

struct ChatServer {
    peer_map: PeerMap,
    addr: String,
    tls_config: Option<TlsConfig>,
}

impl ChatServer {
    pub fn new(addr: String) -> Self {
        let peer_map = Arc::new(Mutex::new(HashMap::new()));
        ChatServer {
            peer_map,
            addr,
            tls_config: None,
        }
    }

    pub fn enable_tls(&mut self) -> Result<()> {
        self.tls_config = Some(TlsConfig::new()?);
        Ok(())
    }

    pub async fn run(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.addr).await?;
        println!("WebSocket server listening on: {}", self.addr);

        while let Ok((stream, addr)) = listener.accept().await {
            let handler = ConnectionHandler::new(self.peer_map.clone());
            let tls_config = self.tls_config.as_ref().map(|c| c.get_acceptor());
            self.spawn_connection_handler(stream, addr, handler, tls_config);
        }

        Ok(())
    }

    fn spawn_connection_handler(
        &self,
        stream: TcpStream,
        addr: SocketAddr,
        handler: ConnectionHandler,
        tls_acceptor: Option<TokioTlsAcceptor>,
    ) {
        tokio::spawn(async move {
            let ws_stream = match tls_acceptor {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => match tokio_tungstenite::accept_async(tls_stream).await {
                        Ok(ws) => Stream::Tls(ws),
                        Err(e) => {
                            println!("Error during WebSocket handshake for TLS connection: {}", e);
                            return;
                        }
                    },
                    Err(e) => {
                        println!("Error during TLS handshake: {}", e);
                        return;
                    }
                },
                None => match tokio_tungstenite::accept_async(stream).await {
                    Ok(ws) => Stream::Plain(ws),
                    Err(e) => {
                        println!("Error during WebSocket handshake for plain connection: {}", e);
                        return;
                    }
                },
            };

            match ws_stream.handle_connection(&handler, addr).await {
                Ok(_) => (),
                Err(e) => eprintln!("Error handling connection for {}: {}", addr, e),
            }
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut server = ChatServer::new("127.0.0.1:8080".to_string());
    server.enable_tls()?;
    server.run().await
}
