use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct FixtureResponse {
    pub status: u16,
    pub body: &'static str,
    pub retry_after: Option<u64>,
    pub(crate) location: Option<String>,
    pub(crate) delay: Duration,
}

impl FixtureResponse {
    pub const fn json(body: &'static str) -> Self {
        Self {
            status: 200,
            body,
            retry_after: None,
            location: None,
            delay: Duration::ZERO,
        }
    }

    pub fn redirect(location: String) -> Self {
        Self {
            status: 302,
            body: "{}",
            retry_after: None,
            location: Some(location),
            delay: Duration::ZERO,
        }
    }

    pub fn partial(body: &'static str, content_range: &'static str) -> Self {
        Self {
            status: 206,
            body,
            retry_after: None,
            // The fixture only uses this private slot as Content-Range for a 206.
            location: Some(content_range.to_owned()),
            delay: Duration::ZERO,
        }
    }

    pub const fn delayed_json(body: &'static str, delay: Duration) -> Self {
        Self {
            status: 200,
            body,
            retry_after: None,
            location: None,
            delay,
        }
    }
}

#[derive(Debug)]
pub struct FixtureServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    maximum_concurrent_requests: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start(responses: Vec<FixtureResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let address = listener.local_addr().expect("fixture server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let active_requests = Arc::new(AtomicUsize::new(0));
        let maximum_concurrent_requests = Arc::new(AtomicUsize::new(0));
        let thread_active_requests = Arc::clone(&active_requests);
        let thread_maximum_concurrent_requests = Arc::clone(&maximum_concurrent_requests);
        let thread = thread::spawn(move || {
            let mut workers = Vec::with_capacity(responses.len());
            for response in responses {
                let (stream, _) = listener.accept().expect("accept fixture request");
                let worker_captured = Arc::clone(&captured);
                let worker_active = Arc::clone(&thread_active_requests);
                let worker_maximum = Arc::clone(&thread_maximum_concurrent_requests);
                workers.push(thread::spawn(move || {
                    handle_request(
                        stream,
                        response,
                        worker_captured,
                        worker_active,
                        worker_maximum,
                    );
                }));
            }
            for worker in workers {
                worker.join().expect("fixture request worker did not panic");
            }
        });

        Self {
            endpoint: format!("http://{address}/w/api.php"),
            requests,
            maximum_concurrent_requests,
            thread: Some(thread),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn maximum_concurrent_requests(&self) -> usize {
        self.maximum_concurrent_requests.load(Ordering::Acquire)
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

fn handle_request(
    mut stream: TcpStream,
    response: FixtureResponse,
    requests: Arc<Mutex<Vec<String>>>,
    active_requests: Arc<AtomicUsize>,
    maximum_concurrent_requests: Arc<AtomicUsize>,
) {
    let request = read_request(&mut stream);
    requests.lock().expect("request lock").push(request);
    let active = active_requests.fetch_add(1, Ordering::AcqRel) + 1;
    maximum_concurrent_requests.fetch_max(active, Ordering::AcqRel);
    thread::sleep(response.delay);
    write_response(&mut stream, &response);
    active_requests.fetch_sub(1, Ordering::AcqRel);
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

fn write_response(stream: &mut TcpStream, response: &FixtureResponse) {
    let reason = match response.status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Fixture Status",
    };
    let retry_after = response
        .retry_after
        .map(|seconds| format!("Retry-After: {seconds}\r\n"))
        .unwrap_or_default();
    let location = response
        .location
        .as_ref()
        .map_or_else(String::new, |value| {
            if response.status == 206 {
                format!("Content-Range: {value}\r\n")
            } else {
                format!("Location: {value}\r\n")
            }
        });
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n{}{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        retry_after,
        location,
        response.body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .expect("write fixture headers");
    stream
        .write_all(response.body.as_bytes())
        .expect("write fixture body");
}
