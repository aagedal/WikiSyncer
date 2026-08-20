use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy, Debug)]
pub struct FixtureResponse {
    body: &'static str,
}

impl FixtureResponse {
    pub const fn json(body: &'static str) -> Self {
        Self { body }
    }
}

#[derive(Debug)]
pub struct FixtureServer {
    endpoint: String,
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start(responses: Vec<FixtureResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("fixture server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                let request = read_request(&mut stream);
                captured.lock().expect("request lock").push(request);
                write_response(&mut stream, response);
            }
        });

        Self {
            endpoint: format!("http://{address}/w/api.php"),
            address,
            requests,
            thread: Some(thread),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn finish(mut self) -> (SocketAddr, Vec<String>) {
        self.thread
            .take()
            .expect("fixture thread")
            .join()
            .expect("fixture server did not panic");
        let requests = Arc::try_unwrap(self.requests)
            .expect("all fixture request handles dropped")
            .into_inner()
            .expect("request lock");
        (self.address, requests)
    }
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).expect("read fixture request");
        assert!(read > 0, "client closed before sending request headers");
        bytes.extend_from_slice(&buffer[..read]);
        assert!(
            bytes.len() <= 64 * 1024,
            "fixture request headers too large"
        );
    }
    String::from_utf8(bytes).expect("request headers are UTF-8 compatible")
}

fn write_response(stream: &mut TcpStream, response: FixtureResponse) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .expect("write fixture headers");
    stream
        .write_all(response.body.as_bytes())
        .expect("write fixture body");
}
