use async_trait::async_trait;
use pkdw::cli::Quality;
use pkdw::download::{download_file, DownloadOptions, DownloadOutcome, LinkSource};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct MockSource {
    url: String,
}

#[async_trait]
impl LinkSource for MockSource {
    async fn link(&self, _force_refresh: bool) -> anyhow::Result<String> {
        Ok(self.url.clone())
    }
}

type RangeLog = Arc<Mutex<Vec<(usize, usize)>>>;

async fn spawn_range_server(payload: Vec<u8>) -> (String, RangeLog) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let payload = Arc::new(payload);
    let log: RangeLog = Arc::new(Mutex::new(Vec::new()));

    tokio::spawn({
        let payload = payload.clone();
        let log = log.clone();
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let payload = payload.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let mut request = Vec::new();
                    loop {
                        let Ok(read) = socket.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&request);
                    let range = text.lines().find_map(|line| {
                        line.split_once(':').and_then(|(key, value)| {
                            key.eq_ignore_ascii_case("range")
                                .then(|| value.trim().to_string())
                        })
                    });
                    let Some(range) = range else {
                        let _ = socket
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    payload.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        let _ = socket.write_all(&payload).await;
                        return;
                    };
                    let spec = range.trim_start_matches("bytes=");
                    let (start, end) = spec.split_once('-').unwrap();
                    let start: usize = start.parse().unwrap();
                    let end: usize = end.parse::<usize>().unwrap().min(payload.len() - 1);
                    log.lock().unwrap().push((start, end));
                    let body = &payload[start..=end];
                    let headers = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                        body.len(),
                        start,
                        end,
                        payload.len()
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        }
    });

    (format!("http://{address}/file.bin"), log)
}

fn options(connections: usize, chunk: u64) -> DownloadOptions {
    DownloadOptions {
        http: reqwest::Client::new(),
        connections,
        chunk_size: chunk,
        rounds: 1,
        quiet: true,
        overwrite: false,
    }
}

#[tokio::test]
async fn downloads_parallel_ranges_and_assembles_exact_file() {
    let payload: Vec<u8> = (0..(1024 * 1024 + 123))
        .map(|index| (index % 251) as u8)
        .collect();
    let (url, _log) = spawn_range_server(payload.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    let source: Arc<dyn LinkSource> = Arc::new(MockSource { url });

    let result = download_file(
        source,
        "test-file",
        "sample.bin",
        Some(payload.len() as u64),
        dir.path(),
        &options(8, 64 * 1024),
        "original",
    )
    .await
    .unwrap();

    match result {
        DownloadOutcome::Complete(path) => {
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes, payload);
        }
        DownloadOutcome::Incomplete { .. } => panic!("download should have completed"),
    }
    assert!(!dir.path().join("sample.bin.part").exists());
    assert!(!dir.path().join("sample.bin.part.pkdw.json").exists());
}

#[tokio::test]
async fn resume_keeps_completed_ranges_and_skips_them() {
    let chunk = 1024 * 1024usize;
    let total = 4 * chunk;
    let payload: Vec<u8> = (0..total).map(|index| (index % 239) as u8).collect();
    let (url, log) = spawn_range_server(payload.clone()).await;
    let dir = tempfile::tempdir().unwrap();

    let part = dir.path().join("resume.bin.part");
    let state_path = dir.path().join("resume.bin.part.pkdw.json");
    let mut sparse = vec![0u8; total];
    sparse[..chunk].copy_from_slice(&payload[..chunk]);
    std::fs::write(&part, &sparse).unwrap();
    std::fs::write(
        &state_path,
        format!(
            r#"{{"version":1,"source_id":"resume-test","size":{total},"representation":"original","completed":[[0,{}]]}}"#,
            chunk - 1
        ),
    )
    .unwrap();

    let source: Arc<dyn LinkSource> = Arc::new(MockSource { url });
    let result = download_file(
        source,
        "resume-test",
        "resume.bin",
        Some(total as u64),
        dir.path(),
        &options(2, chunk as u64),
        "original",
    )
    .await
    .unwrap();

    match result {
        DownloadOutcome::Complete(path) => {
            assert_eq!(std::fs::read(path).unwrap(), payload);
        }
        DownloadOutcome::Incomplete { .. } => panic!("resume should have completed"),
    }

    let requested = log.lock().unwrap().clone();
    assert!(
        requested.iter().all(|(start, _)| *start != 0),
        "the completed first segment was requested again: {requested:?}"
    );
    assert!(!state_path.exists());
    assert!(!part.exists());
}

#[test]
fn quality_keys_are_stable() {
    assert_eq!(Quality::Original.key(), "original");
    assert_eq!(Quality::P1080.key(), "1080p");
}
