use futures_util::{future, pin_mut, SinkExt, StreamExt};
use native_tls::TlsConnector;
use std::error::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio_native_tls::{TlsConnector as TokioTlsConnector, TlsStream};
use tokio_tungstenite::{client_async, tungstenite::protocol::Message, WebSocketStream};
use url::Url;

struct ChatClient {
    url: Url,
    ws_stream: Option<WebSocketStream<TlsStream<TcpStream>>>,
    tls_connector: Option<TlsConnector>,
}

impl ChatClient {
    pub fn new(server_addr: &str) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(&format!("wss://{}", server_addr))?;
        Ok(ChatClient {
            url,
            ws_stream: None,
            tls_connector: None,
        })
    }

    pub fn with_tls_config(&mut self, accept_invalid_certs: bool) -> Result<(), Box<dyn Error>> {
        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(accept_invalid_certs) // Accept self-signed certificates
            .build()?;
        self.tls_connector = Some(connector);
        Ok(())
    }

    pub async fn connect(&mut self) -> Result<(), Box<dyn Error>> {
        let connector = if let Some(tls) = self.tls_connector.as_ref() {
            tls.clone()
        } else {
            return Err("TLS not configured".into());
        };

        let try_socket = TcpStream::connect(format!(
            "{}:{}",
            self.url.host_str().unwrap_or("127.0.0.1"),
            self.url.port_or_known_default().unwrap_or(8080)
        ))
        .await?;

        let socket = TokioTlsConnector::from(connector)
            .connect(self.url.host_str().unwrap_or("localhost"), try_socket)
            .await?;

        let (ws_stream, response) = client_async(self.url.to_string(), socket).await?;

        println!("Connected to server");
        println!("Response HTTP code: {}", response.status());
        self.ws_stream = Some(ws_stream);
        Ok(())
    }

    pub async fn run(&mut self) -> Result<(), Box<dyn Error>> {
        let ws_stream = self.ws_stream.take().ok_or("Not connected to server")?;
        let (write, read) = ws_stream.split();

        let stdin_handler = Self::handle_user_input(write);
        let message_handler = Self::handle_server_messages(read);

        pin_mut!(stdin_handler, message_handler);
        match future::select(stdin_handler, message_handler).await {
            future::Either::Left((result, _)) => {
                if let Err(e) = result {
                    eprintln!("Error in stdin channel: {}", e);
                }
            }
            future::Either::Right((_, _)) => {
                println!("WebSocket connection closed");
            }
        }

        Ok(())
    }

    async fn handle_user_input<S>(mut write: S) -> Result<(), Box<dyn Error>>
    where
        S: SinkExt<Message> + Unpin,
        S::Error: Error + Send + Sync + 'static,
    {
        let mut stdin = BufReader::new(tokio::io::stdin());
        let mut line = String::new();

        loop {
            line.clear();
            match stdin.read_line(&mut line).await {
                Ok(n) if n == 0 || line.trim().is_empty() => break,
                Ok(_) => {
                    write
                        .send(Message::Text(line.trim().to_string()))
                        .await
                        .map_err(|e| Box::new(e) as Box<dyn Error>)?;
                }
                Err(e) => {
                    eprintln!("Error reading from stdin: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_server_messages(
        read: impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    ) {
        read.for_each(|message| async {
            match message {
                Ok(msg) => {
                    if let Message::Text(text) = msg {
                        println!("Received: {}", text);
                    }
                }
                Err(e) => {
                    eprintln!("Error receiving message: {}", e);
                }
            }
        })
        .await
    }
}

#[tokio::main]
async fn main() {
    let mut client = match ChatClient::new("127.0.0.1:8080") {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Failed to create client: {}", e);
            return;
        }
    };

    // Configure TLS to accept self-signed certificates
    if let Err(e) = client.with_tls_config(true) {
        eprintln!("Failed to configure TLS: {}", e);
        return;
    }

    if let Err(e) = client.connect().await {
        eprintln!("Failed to connect to server: {}", e);
        return;
    }

    if let Err(e) = client.run().await {
        eprintln!("Error running client: {}", e);
    }
}
