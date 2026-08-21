use std::process::Command;
use std::thread;

use wikisync_store::{Library, ObjectKind};
use wikisyncd::{ApplicationHandler, Client, Daemon};

#[test]
fn verify_forwards_to_the_daemon_that_owns_the_library_writer() {
    let temporary = tempfile::tempdir().expect("temporary library");
    let mut library = Library::open(temporary.path()).expect("initialize library");
    library
        .put_bytes(ObjectKind::Wikitext, b"canonical fixture")
        .expect("store fixture object");
    drop(library);

    let handler = ApplicationHandler::new(temporary.path()).expect("application handler");
    let daemon = Daemon::bind(temporary.path(), handler).expect("bind daemon");
    let daemon_thread = thread::spawn(move || daemon.run());

    let output = Command::new(env!("CARGO_BIN_EXE_wikisync"))
        .args([
            "--library",
            temporary.path().to_str().expect("UTF-8 fixture path"),
            "verify",
            "--full",
        ])
        .output()
        .expect("run CLI");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
    assert!(stdout.contains("verification-complete"));
    assert!(stdout.contains("scope=full"));
    assert!(stdout.contains("fully_verified=true"));

    Client::for_library(temporary.path())
        .expect("daemon client")
        .shutdown()
        .expect("shutdown daemon");
    daemon_thread
        .join()
        .expect("join daemon")
        .expect("daemon result");
}
