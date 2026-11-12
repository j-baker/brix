//! Git's smart HTTP protocol over brix's HTTP client: an implementation of
//! gix's [`Http`] transport backend on the [`HttpClient`]'s `ureq` agent.
//!
//! Nix fetches Git repositories with the `git` program, which uses curl,
//! so Git and tarball downloads trust the same certificates (Nix's
//! `ssl-cert-file`, see [`HttpClient::with_ca_file`]) and see the same
//! proxies. gix's own HTTP backends build their clients themselves and
//! cannot be given the certificates; this one shares the agent instead.
//!
//! As in gix's `reqwest` backend, each request runs on a thread of its
//! own that streams the body to post from a pipe the caller writes, and
//! the response's headers and body into pipes the caller reads. A
//! redirect of the first request moves the base URL of the ones after it,
//! as Git (`http.followRedirects = initial`) and gix do.

use std::any::Any;
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use gix::features::io::pipe;
use gix::protocol::transport::client::http::{
    self as gix_http, options::FollowRedirects, GetResponse, Http, PostBodyDataKind, PostResponse,
};
use ureq::ResponseExt;

use super::http::HttpClient;

/// The [`Http`] backend: one per connection, as gix makes them.
pub struct UreqHttp {
    agent: ureq::Agent,
    /// curl's error for an HTTPS transfer the CA file makes impossible.
    ca_error: Option<String>,
    follow_redirects: FollowRedirects,
    /// The base URL the first request was redirected to, if it was.
    redirected_base: Arc<Mutex<Option<String>>>,
}

impl UreqHttp {
    pub fn new(client: &HttpClient, url: &str) -> UreqHttp {
        UreqHttp {
            agent: client.agent().clone(),
            ca_error: client.ca_error(url).map(str::to_owned),
            follow_redirects: FollowRedirects::Initial,
            redirected_base: Arc::new(Mutex::new(None)),
        }
    }

    fn request(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        upload: Option<PostBodyDataKind>,
    ) -> Result<PostResponse<pipe::Reader, pipe::Reader, pipe::Writer>, gix_http::Error> {
        if let Some(e) = &self.ca_error {
            return Err(gix_http::Error::Detail {
                description: e.clone(),
            });
        }
        let tail = url
            .strip_prefix(base_url)
            .expect("invariant: gix passes a URL below its base URL")
            .to_owned();
        let effective = match &*self.redirected_base.lock().expect("not poisoned") {
            Some(base) => format!("{base}{tail}"),
            None => url.to_owned(),
        };
        let follow = match self.follow_redirects {
            FollowRedirects::All => true,
            FollowRedirects::Initial => {
                // Only the first request of a connection may be redirected.
                self.follow_redirects = FollowRedirects::None;
                true
            }
            FollowRedirects::None => false,
        };
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .filter_map(|line| {
                let (name, value) = line.as_ref().split_once(':')?;
                Some((name.trim().to_owned(), value.trim().to_owned()))
            })
            .collect();

        let (post_tx, post_rx) = pipe::unidirectional(0);
        let (headers_tx, headers_rx) = pipe::unidirectional(0);
        let (body_tx, body_rx) = pipe::unidirectional(0);
        let agent = self.agent.clone();
        let redirected_base = self.redirected_base.clone();
        std::thread::Builder::new()
            .name("brix-git-http".into())
            .spawn(move || {
                let request = Request {
                    agent,
                    url: effective,
                    tail,
                    headers,
                    upload,
                    follow,
                    redirected_base,
                };
                run_worker(headers_tx, body_tx, |headers, body| {
                    request.run(post_rx, headers, body);
                });
            })
            .map_err(|source| gix_http::Error::InitHttpClient {
                source: Box::new(source),
            })?;
        Ok(PostResponse {
            post_body: post_tx,
            headers: headers_rx,
            body: body_rx,
        })
    }
}

/// The pipes own the request's lifetime: dropping their readers releases a
/// worker blocked on output. Do not join here or on transport drop, since the
/// caller may still own the upload writer or unread response pipes. A blocking
/// network operation still has to return before cancellation can be observed.
/// Report an unwind on the active output pipe instead of silently closing it.
fn run_worker(
    headers: pipe::Writer,
    mut body: pipe::Writer,
    run: impl FnOnce(&mut Option<pipe::Writer>, &mut pipe::Writer),
) {
    let mut headers = Some(headers);
    // The request is discarded after unwinding; only its output pipes are used
    // again, to report failure. No partially updated request is resumed.
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| run(&mut headers, &mut body))) {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        let error = std::io::Error::other(format!("Git HTTP worker panicked: {message}"));
        // Headers close before body streaming starts, so only one of these
        // pipes can be the caller's active reader. Disconnection is cancellation.
        let output = headers.as_mut().unwrap_or(&mut body);
        let _ = output.channel.send(Err(error));
    }
}

/// One request, on its thread.
struct Request {
    agent: ureq::Agent,
    url: String,
    /// What follows the base URL in `url`.
    tail: String,
    headers: Vec<(String, String)>,
    upload: Option<PostBodyDataKind>,
    follow: bool,
    redirected_base: Arc<Mutex<Option<String>>>,
}

impl Request {
    fn run(
        self,
        mut post_rx: pipe::Reader,
        headers_tx: &mut Option<pipe::Writer>,
        body_tx: &mut pipe::Writer,
    ) {
        let response = match self.send(&mut post_rx) {
            Ok(response) => response,
            Err(e) => {
                // Nobody may be reading any more, which is no error here.
                let _ = headers_tx
                    .as_mut()
                    .expect("headers are open")
                    .channel
                    .send(Err(e));
                return;
            }
        };
        let final_url = response.get_uri().to_string();
        if final_url != self.url {
            if let Some(base) = final_url.strip_suffix(&self.tail) {
                *self.redirected_base.lock().expect("not poisoned") = Some(base.to_owned());
            }
        }
        let (parts, body) = response.into_parts();
        let mut header_lines = Vec::new();
        for (name, value) in &parts.headers {
            header_lines.extend_from_slice(name.as_str().as_bytes());
            header_lines.push(b':');
            header_lines.extend_from_slice(value.as_bytes());
            header_lines.push(b'\n');
        }
        // As above: a reader that went away is no error here.
        if headers_tx
            .as_mut()
            .expect("headers are open")
            .write_all(&header_lines)
            .is_err()
        {
            return;
        }
        drop(headers_tx.take());
        let mut reader = body.into_with_config().limit(u64::MAX).reader();
        if let Err(e) = std::io::copy(&mut reader, body_tx) {
            let _ = body_tx.channel.send(Err(e));
        }
    }

    /// Sends the request, the body to post read from `post_rx`, and
    /// checks the status as gix's backends do: 401 is
    /// `PermissionDenied`, anything else but success an error of its own.
    fn send(
        &self,
        post_rx: &mut pipe::Reader,
    ) -> std::io::Result<ureq::http::Response<ureq::Body>> {
        let other = |e: ureq::Error| std::io::Error::other(e.to_string());
        // Git follows at most curl's default of redirects, the first
        // request's only.
        let max_redirects = if self.follow { 50 } else { 0 };
        let response = match self.upload {
            None => {
                let mut req = self
                    .agent
                    .get(&self.url)
                    .config()
                    .max_redirects(max_redirects)
                    .build();
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                req.call().map_err(other)?
            }
            Some(kind) => {
                let mut req = self
                    .agent
                    .post(&self.url)
                    .config()
                    .max_redirects(max_redirects)
                    .build();
                for (name, value) in &self.headers {
                    req = req.header(name.as_str(), value.as_str());
                }
                match kind {
                    PostBodyDataKind::BoundedAndFitsIntoMemory => {
                        let mut buf = Vec::new();
                        post_rx.read_to_end(&mut buf)?;
                        req.send(&buf[..]).map_err(other)?
                    }
                    PostBodyDataKind::Unbounded => req
                        .send(ureq::SendBody::from_reader(post_rx))
                        .map_err(other)?,
                }
            }
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let kind = if status.as_u16() == 401 {
            std::io::ErrorKind::PermissionDenied
        } else if status.is_server_error() {
            std::io::ErrorKind::ConnectionAborted
        } else {
            std::io::ErrorKind::Other
        };
        Err(std::io::Error::new(
            kind,
            format!("Received HTTP status {}", status.as_str()),
        ))
    }
}

impl Http for UreqHttp {
    type Headers = pipe::Reader;
    type ResponseBody = pipe::Reader;
    type PostBody = pipe::Writer;

    fn get(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<GetResponse<Self::Headers, Self::ResponseBody>, gix_http::Error> {
        self.request(url, base_url, headers, None).map(Into::into)
    }

    fn post(
        &mut self,
        url: &str,
        base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        body: PostBodyDataKind,
    ) -> Result<PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, gix_http::Error>
    {
        self.request(url, base_url, headers, Some(body))
    }

    fn configure(
        &mut self,
        config: &dyn Any,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        // Of Git's `http.*` settings only the redirect policy is used: the
        // rest (proxies, timeouts, certificates) are brix's, as they are
        // curl's for the `git` that Nix runs.
        if let Some(options) = config.downcast_ref::<gix_http::Options>() {
            self.follow_redirects = options.follow_redirects;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_before_headers_is_reported_to_the_header_reader() {
        let (headers_tx, mut headers_rx) = pipe::unidirectional(0);
        let (body_tx, _body_rx) = pipe::unidirectional(0);
        let worker = std::thread::spawn(move || {
            run_worker(headers_tx, body_tx, |_, _| panic!("before headers"));
        });
        let error = headers_rx.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error
            .to_string()
            .contains("Git HTTP worker panicked: before headers"));
        worker.join().unwrap();
    }

    #[test]
    fn panic_during_body_is_not_a_successful_truncated_response() {
        let (headers_tx, mut headers_rx) = pipe::unidirectional(0);
        let (body_tx, mut body_rx) = pipe::unidirectional(0);
        let worker = std::thread::spawn(move || {
            run_worker(headers_tx, body_tx, |headers, body| {
                headers
                    .as_mut()
                    .unwrap()
                    .write_all(b"content-type: test\n")
                    .unwrap();
                drop(headers.take());
                body.write_all(b"partial").unwrap();
                panic!("during body");
            });
        });
        let mut headers = String::new();
        headers_rx.read_to_string(&mut headers).unwrap();
        assert_eq!(headers, "content-type: test\n");
        let mut prefix = [0; 7];
        body_rx.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"partial");
        let error = body_rx.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(error
            .to_string()
            .contains("Git HTTP worker panicked: during body"));
        worker.join().unwrap();
    }

    #[test]
    fn abandoned_response_does_not_block_panic_reporting() {
        let (headers_tx, headers_rx) = pipe::unidirectional(0);
        let (body_tx, body_rx) = pipe::unidirectional(0);
        drop(headers_rx);
        drop(body_rx);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            run_worker(headers_tx, body_tx, |_, _| panic!("abandoned request"));
            done_tx.send(()).unwrap();
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.join().unwrap();
    }
}
