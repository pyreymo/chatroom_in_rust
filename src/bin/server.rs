use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_channel::mpsc::{unbounded, UnboundedSender};
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

type Tx = UnboundedSender<Message>;
type PeerMap = Arc<Mutex<HashMap<SocketAddr, Tx>>>;

struct ChatServer {
    peer_map: PeerMap,
    addr: String,
}

impl ChatServer {
    /// Create a new ChatServer instance
    pub fn new(addr: String) -> Self {
        ChatServer {
            peer_map: Arc::new(Mutex::new(HashMap::new())),
            addr,
        }
    }

    /// Start the chat server
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(&self.addr).await?;
        println!("WebSocket server listening on: {}", self.addr);

        while let Ok((stream, addr)) = listener.accept().await {
            let peer_map = self.peer_map.clone();
            tokio::spawn(Self::handle_connection(peer_map, stream, addr));
        }

        Ok(())
    }

    /// Handle an individual WebSocket connection
    async fn handle_connection(peer_map: PeerMap, raw_stream: TcpStream, addr: SocketAddr) {
        println!("Incoming TCP connection from: {}", addr);

        let ws_stream = match tokio_tungstenite::accept_async(raw_stream).await {
            Ok(ws_stream) => {
                println!("WebSocket connection established with: {}", addr);
                ws_stream
            }
            Err(e) => {
                println!("Error during WebSocket handshake for {}: {}", addr, e);
                return;
            }
        };

        let (tx, rx) = unbounded();
        peer_map.lock().await.insert(addr, tx);

        let (outgoing, incoming) = ws_stream.split();

        let broadcast_incoming = Self::handle_incoming_messages(peer_map.clone(), addr, incoming);
        let receive_from_others = rx.map(Ok).forward(outgoing);

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

    /// Handle incoming messages from a client
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

    let server = ChatServer::new("127.0.0.1:8080".to_string());
    if let Err(e) = server.run().await {
        eprintln!("Error running server: {}", e);
    }
}
