//! HTTP clients for AcoustID, MusicBrainz and the Cover Art Archive.

use std::io::Read;
use std::thread::sleep;
use std::time::{Duration, Instant};

use lofty::picture::Picture;
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder};
use serde::Deserialize;

use crate::{Error, Result};

const USER_AGENT: &str = concat!(
    "Cantus/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/AallynReed/cantus )"
);
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub acoustid: String,
    pub musicbrainz: String,
    pub cover_art: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            acoustid: "https://api.acoustid.org/v2/lookup".into(),
            musicbrainz: "https://musicbrainz.org/ws/2".into(),
            cover_art: "https://coverartarchive.org".into(),
        }
    }
}

/// Spaces calls out; every caller shares one so a service's rate limit holds overall.
#[derive(Debug)]
struct Limiter {
    interval: Duration,
    next: Option<Instant>,
}

impl Limiter {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: None,
        }
    }

    /// Reserves the next slot and returns how long to wait for it.
    fn reserve(&mut self, now: Instant) -> Duration {
        let at = self.next.map_or(now, |next| next.max(now));
        self.next = Some(at + self.interval);
        at - now
    }

    fn wait(&mut self) {
        sleep(self.reserve(Instant::now()));
    }
}

pub struct Lookup {
    http: Client,
    endpoints: Endpoints,
    acoustid_key: Option<String>,
    musicbrainz_limit: Limiter,
    acoustid_limit: Limiter,
    retry_wait: Duration,
}

impl Lookup {
    pub fn new(endpoints: Endpoints, acoustid_key: Option<String>) -> Result<Self> {
        let https_only = endpoints.musicbrainz.starts_with("https:");
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .https_only(https_only)
            .build()
            .map_err(|e| Error::InvalidInput(format!("cannot start the HTTP client: {e}")))?;
        Ok(Self {
            http,
            endpoints,
            acoustid_key: acoustid_key.filter(|k| !k.trim().is_empty()),
            musicbrainz_limit: Limiter::new(Duration::from_secs(1)),
            acoustid_limit: Limiter::new(Duration::from_millis(334)),
            retry_wait: Duration::from_secs(1),
        })
    }

    pub(crate) fn has_acoustid_key(&self) -> bool {
        self.acoustid_key.is_some()
    }

    /// Recording ids matching a fingerprint, with scores from 0 to 100, best first.
    pub(crate) fn acoustid(
        &mut self,
        fingerprint: &str,
        duration_secs: u32,
    ) -> Result<Vec<(String, u8)>, String> {
        let key = self.acoustid_key.clone().ok_or("no AcoustID key set")?;
        let form = [
            ("client", key),
            ("duration", duration_secs.to_string()),
            ("fingerprint", fingerprint.to_owned()),
            ("meta", "recordingids".to_owned()),
            ("format", "json".to_owned()),
        ];
        let url = self.endpoints.acoustid.clone();
        let http = self.http.clone();
        let body = send(&mut self.acoustid_limit, self.retry_wait, || {
            http.post(&url).form(&form)
        })?
        .ok_or("AcoustID returned not found")?;
        parse_acoustid(&body)
    }

    /// A MusicBrainz GET. `None` means not found.
    pub(crate) fn musicbrainz(
        &mut self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Vec<u8>>, String> {
        let url = format!("{}/{path}", self.endpoints.musicbrainz);
        let http = self.http.clone();
        send(&mut self.musicbrainz_limit, self.retry_wait, || {
            http.get(&url).query(query).query(&[("fmt", "json")])
        })
    }

    /// The front cover of a release, if it has one.
    pub fn cover_art(&mut self, release_mbid: &str) -> Option<Vec<u8>> {
        let url = format!(
            "{}/release/{release_mbid}/front-500",
            self.endpoints.cover_art
        );
        let http = self.http.clone();
        send(&mut self.musicbrainz_limit, self.retry_wait, || {
            http.get(&url)
        })
        .ok()
        .flatten()
        .filter(|bytes| Picture::from_reader(&mut bytes.as_slice()).is_ok())
    }
}

/// Sends with retries on network errors, 429 and 503. `Ok(None)` means 404.
fn send(
    limiter: &mut Limiter,
    retry_wait: Duration,
    request: impl Fn() -> RequestBuilder,
) -> Result<Option<Vec<u8>>, String> {
    let mut last_error = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        limiter.wait();
        let backoff = retry_wait * 2u32.pow(attempt);
        match request().send() {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => return Ok(None),
            Ok(response) if response.status().is_success() => {
                let mut body = Vec::new();
                response
                    .take(MAX_RESPONSE_BYTES)
                    .read_to_end(&mut body)
                    .map_err(|e| e.to_string())?;
                return Ok(Some(body));
            }
            Ok(response)
                if matches!(
                    response.status(),
                    StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                ) =>
            {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .map_or(backoff, Duration::from_secs);
                last_error = format!("server busy ({})", response.status());
                sleep(wait.min(MAX_RETRY_WAIT));
            }
            Ok(response) => return Err(format!("server error ({})", response.status())),
            Err(err) => {
                last_error = err.to_string();
                sleep(backoff.min(MAX_RETRY_WAIT));
            }
        }
    }
    Err(last_error)
}

fn parse_acoustid(body: &[u8]) -> Result<Vec<(String, u8)>, String> {
    #[derive(Deserialize)]
    struct Response {
        status: String,
        #[serde(default)]
        error: Option<Message>,
        #[serde(default)]
        results: Vec<Match>,
    }
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    #[derive(Deserialize)]
    struct Match {
        score: f32,
        #[serde(default)]
        recordings: Vec<Id>,
    }
    #[derive(Deserialize)]
    struct Id {
        id: String,
    }

    let response: Response = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if response.status != "ok" {
        let reason = response.error.map_or(response.status, |e| e.message);
        return Err(format!("AcoustID: {reason}"));
    }
    let mut matches: Vec<(String, u8)> = response
        .results
        .into_iter()
        .flat_map(|m| {
            let score = (m.score.clamp(0.0, 1.0) * 100.0).round() as u8;
            m.recordings.into_iter().map(move |r| (r.id, score))
        })
        .collect();
    matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    Ok(matches)
}

/// A local HTTP server for tests. No test touches the real network.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    /// `respond` gets "METHOD /path?query body" and returns status and body.
    pub fn start(respond: impl Fn(&str) -> (u16, Vec<u8>) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let parts: Vec<_> = first.split_whitespace().collect();
                let request = format!(
                    "{} {} {}",
                    parts[0],
                    parts[1],
                    String::from_utf8_lossy(&body)
                );
                log.lock().unwrap().push(request.clone());
                let (status, body) = respond(&request);
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Server { url, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(url: &str) -> Lookup {
        let endpoints = Endpoints {
            acoustid: format!("{url}/acoustid"),
            musicbrainz: url.into(),
            cover_art: url.into(),
        };
        let mut lookup = Lookup::new(endpoints, Some("key".into())).unwrap();
        lookup.musicbrainz_limit = Limiter::new(Duration::ZERO);
        lookup.acoustid_limit = Limiter::new(Duration::ZERO);
        lookup.retry_wait = Duration::ZERO;
        lookup
    }

    #[test]
    fn limiter_spaces_ten_calls_nine_intervals_apart() {
        let mut limiter = Limiter::new(Duration::from_secs(1));
        let now = Instant::now();
        let waits: Vec<_> = (0..10).map(|_| limiter.reserve(now)).collect();
        assert_eq!(waits[0], Duration::ZERO);
        assert_eq!(waits[9], Duration::from_secs(9));
    }

    #[test]
    fn limiter_does_not_wait_after_idle_time() {
        let mut limiter = Limiter::new(Duration::from_secs(1));
        let now = Instant::now();
        limiter.reserve(now);
        assert_eq!(
            limiter.reserve(now + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn busy_server_is_retried_then_succeeds() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let server = fake::start(move |_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                (503, vec![])
            } else {
                (200, b"{}".to_vec())
            }
        });
        let body = lookup(&server.url).musicbrainz("recording/x", &[]).unwrap();
        assert_eq!(body.as_deref(), Some(&b"{}"[..]));
        assert_eq!(server.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn a_server_that_stays_busy_is_an_error_after_a_few_tries() {
        let server = fake::start(|_| (503, vec![]));
        assert!(lookup(&server.url).musicbrainz("x", &[]).is_err());
        assert_eq!(server.requests.lock().unwrap().len(), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn not_found_is_none_and_other_errors_are_not_retried() {
        let server = fake::start(|r| {
            if r.contains("missing") {
                (404, vec![])
            } else {
                (500, vec![])
            }
        });
        let mut lookup = lookup(&server.url);
        assert_eq!(lookup.musicbrainz("missing", &[]).unwrap(), None);
        assert!(lookup.musicbrainz("broken", &[]).is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn user_agent_names_the_project() {
        assert!(USER_AGENT.starts_with("Cantus/"));
        assert!(USER_AGENT.contains("https://"));
    }

    #[test]
    fn acoustid_errors_and_scores_are_parsed() {
        let error = br#"{"status":"error","error":{"code":4,"message":"invalid API key"}}"#;
        assert_eq!(
            parse_acoustid(error).unwrap_err(),
            "AcoustID: invalid API key"
        );
        let ok = br#"{"status":"ok","results":[
            {"score":0.5,"recordings":[{"id":"b"}]},
            {"score":0.99,"recordings":[{"id":"a"}]},
            {"score":0.9}]}"#;
        assert_eq!(
            parse_acoustid(ok).unwrap(),
            [("a".to_owned(), 99), ("b".to_owned(), 50)]
        );
    }
}
