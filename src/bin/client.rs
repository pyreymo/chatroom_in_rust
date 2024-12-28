// 导入必要的异步编程工具
use futures_util::{future, pin_mut, SinkExt, StreamExt}; // 用于处理异步流操作
use tokio::io::AsyncBufReadExt; // 异步读取缓冲区
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message}; // WebSocket客户端功能
use url::Url; // URL解析工具

/// WebSocket客户端主函数
#[tokio::main]
async fn main() {
    // 解析WebSocket服务器地址
    let url = Url::parse("ws://127.0.0.1:8080").unwrap();

    // 尝试连接到WebSocket服务器
    let (ws_stream, _response) = match connect_async(url).await {
        Ok((ws_stream, response)) => {
            println!("Connected to server");
            println!("Response HTTP code: {}", response.status());
            (ws_stream, response)
        }
        Err(e) => {
            eprintln!("Failed to connect to server: {}", e);
            return;
        }
    };

    // 将WebSocket流分割为读写两部分
    let (mut write, read) = ws_stream.split();

    // 创建标准输入的缓冲读取器
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    // 创建用于处理用户输入的异步任务
    let stdin_channel = tokio::spawn(async move {
        loop {
            line.clear();
            // 从标准输入读取一行
            match stdin.read_line(&mut line).await {
                // 如果读取到EOF或空行则退出
                Ok(n) if n == 0 || line.trim().is_empty() => break,
                // 成功读取到内容则发送消息
                Ok(_) => {
                    if let Err(e) = write.send(Message::Text(line.trim().to_string())).await {
                        eprintln!("Error sending message: {}", e);
                        break;
                    }
                }
                // 处理读取错误
                Err(e) => {
                    eprintln!("Error reading from stdin: {}", e);
                    break;
                }
            }
        }
    });

    // 创建用于接收服务器消息的异步流处理器
    let ws_to_stdout = {
        read.for_each(|message| async {
            match message {
                Ok(msg) => {
                    // 只处理文本消息
                    if let Message::Text(text) = msg {
                        println!("Received: {}", text);
                    }
                }
                Err(e) => {
                    eprintln!("Error receiving message: {}", e);
                }
            }
        })
    };

    // 同时处理用户输入和服务器消息
    pin_mut!(ws_to_stdout);
    match future::select(stdin_channel, ws_to_stdout).await {
        // 处理用户输入通道的结果
        future::Either::Left((result, _)) => {
            if let Err(e) = result {
                eprintln!("Error in stdin channel: {}", e);
            }
        }
        // 处理WebSocket连接关闭的情况
        future::Either::Right((_, _)) => {
            println!("WebSocket connection closed");
        }
    }
}
