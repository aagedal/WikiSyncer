use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Clone, Debug)]
pub struct FixtureResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: &'static str,
    gate: Option<FixtureGate>,
}

impl FixtureResponse {
    pub fn json(body: impl AsRef<str>) -> Self {
        Self {
            status: 200,
            body: body.as_ref().as_bytes().to_vec(),
            content_type: "application/json",
            gate: None,
        }
    }

    pub fn bytes(body: impl Into<Vec<u8>>, content_type: &'static str) -> Self {
        Self {
            status: 200,
            body: body.into(),
            content_type,
            gate: None,
        }
    }

    pub fn status_json(status: u16, body: impl AsRef<str>) -> Self {
        Self {
            status,
            body: body.as_ref().as_bytes().to_vec(),
            content_type: "application/json",
            gate: None,
        }
    }

    pub fn blocked(mut self, gate: FixtureGate) -> Self {
        self.gate = Some(gate);
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct FixtureGate {
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl FixtureGate {
    pub fn wait_until_requested(&self, timeout: std::time::Duration) -> bool {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("fixture gate lock");
        if !state.0 {
            let (updated, _) = changed
                .wait_timeout(state, timeout)
                .expect("fixture gate wait");
            state = updated;
        }
        state.0
    }

    pub fn release(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("fixture gate lock");
        state.1 = true;
        changed.notify_all();
    }

    fn arrive_and_wait(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("fixture gate lock");
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).expect("fixture gate wait");
        }
    }
}

#[derive(Debug)]
pub struct FixtureServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start(responses: Vec<FixtureResponse>) -> Self {
        Self::start_generated(move |_| responses)
    }

    pub fn start_generated(responses: impl FnOnce(&str) -> Vec<FixtureResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("fixture server address");
        let endpoint = format!("http://{address}/w/api.php");
        let responses = responses(&endpoint);
        let response_endpoint = endpoint.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let thread = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept fixture request");
                let request = read_request(&mut stream);
                captured.lock().expect("request lock").push(request);
                write_response(&mut stream, response, &response_endpoint);
            }
        });

        Self {
            endpoint,
            requests,
            thread: Some(thread),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn finish(mut self) -> Vec<String> {
        self.thread
            .take()
            .expect("fixture thread")
            .join()
            .expect("fixture server did not panic");
        Arc::try_unwrap(self.requests)
            .expect("all fixture request handles dropped")
            .into_inner()
            .expect("request lock")
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

fn write_response(stream: &mut TcpStream, response: FixtureResponse, endpoint: &str) {
    if let Some(gate) = &response.gate {
        gate.arrive_and_wait();
    }
    let body = if response.content_type == "application/json" {
        String::from_utf8(response.body)
            .expect("JSON fixture is UTF-8")
            .replace("{{ENDPOINT}}", endpoint)
            .into_bytes()
    } else {
        response.body
    };
    let headers = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.content_type,
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .expect("write fixture headers");
    stream.write_all(&body).expect("write fixture body");
}
