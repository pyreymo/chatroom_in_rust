// 导入标准库中的必要组件
use std::collections::HashMap; // 用于存储连接的客户端
use std::net::SocketAddr; // 用于表示网络地址
use std::sync::Arc; // 用于共享所有权的智能指针

// 导入异步编程相关的依赖
use futures_channel::mpsc::{unbounded, UnboundedSender}; // 用于异步消息通道
use futures_util::{future, pin_mut, stream::TryStreamExt, StreamExt}; // 异步工具集
use tokio::net::{TcpListener, TcpStream}; // 异步网络功能
use tokio::sync::Mutex; // 异步互斥锁
use tokio_tungstenite::tungstenite::Message; // WebSocket消息类型

// 类型别名定义
type Tx = UnboundedSender<Message>; // 消息发送器类型
type PeerMap = Arc<Mutex<HashMap<SocketAddr, Tx>>>; // 客户端连接映射表类型

/// 处理单个WebSocket连接的异步函数
async fn handle_connection(peer_map: PeerMap, raw_stream: TcpStream, addr: SocketAddr) {
    println!("Incoming TCP connection from: {}", addr);

    // 尝试将TCP连接升级为WebSocket连接
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

    // 创建用于发送消息的通道
    let (tx, rx) = unbounded();
    // 将新连接的客户端添加到连接映射表中
    peer_map.lock().await.insert(addr, tx);

    // 将WebSocket流分割为发送和接收部分
    let (outgoing, incoming) = ws_stream.split();

    // 处理接收到的消息并广播给其他客户端
    let broadcast_incoming = {
        let peer_map = peer_map.clone();
        incoming.try_for_each(move |msg| {
            let peer_map = peer_map.clone();
            async move {
                match msg.to_text() {
                    Ok(text) => {
                        println!("Received message from {}: {}", addr, text);
                        let peers = peer_map.lock().await;

                        // 过滤出除发送者外的所有客户端
                        let broadcast_recipients = peers
                            .iter()
                            .filter(|(peer_addr, _)| peer_addr != &&addr)
                            .map(|(_, ws_sink)| ws_sink);

                        // 向所有其他客户端广播消息
                        for recp in broadcast_recipients {
                            if let Err(e) = recp.unbounded_send(msg.clone()) {
                                println!("Error broadcasting message: {}", e);
                            }
                        }
                    }
                    Err(e) => println!("Error processing message from {}: {}", addr, e),
                }
                Ok(())
            }
        })
    };

    // 处理从其他客户端接收到的消息
    let receive_from_others = rx.map(Ok).forward(outgoing);

    // 同时处理消息的发送和接收
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

    // 客户端断开连接时的清理工作
    println!("{} disconnected", &addr);
    peer_map.lock().await.remove(&addr);
}

/// 主函数：启动WebSocket服务器
#[tokio::main]
async fn main() {
    // 初始化日志系统
    env_logger::init();

    // 设置服务器监听地址
    let addr = "127.0.0.1:8080";
    // 创建用于存储所有客户端连接的共享状态
    let state = PeerMap::new(Mutex::new(HashMap::new()));

    // 绑定TCP监听器到指定地址
    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket.expect("Failed to bind");
    println!("Listening on: {}", addr);

    // 持续接受新的连接请求
    while let Ok((stream, addr)) = listener.accept().await {
        // 为每个新连接创建一个新的任务
        tokio::spawn(handle_connection(state.clone(), stream, addr));
    }
}
