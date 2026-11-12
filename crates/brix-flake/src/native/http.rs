//! HTTP downloads (`src/libstore/filetransfer.cc`, reduced to what
//! fetching needs: GET with headers, redirects, the `Link` header, the
//! connection timeout, and retries of transient failures).

use std::cell::Cell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::input::FetchSettings;
use crate::native::netrc::Netrc;
use crate::progress::{Activity, ActivityKind, NoProgress, Progress};
use crate::{err, Error, Result};
use brix_store::url::{parse_url, url_path_to_path};
use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};

/// A downloaded resource.
#[derive(Debug, Clone)]
pub struct Download {
    pub data: Vec<u8>,
    /// `Link: <…>; rel="immutable"`, which GitHub-style tarball endpoints
    /// use to point at a permanent URL for the same content.
    pub immutable_url: Option<String>,
}

/// `filetransfer-retry-delay-rate-limited`: the first delay after a 429 or
/// 503, which say the server is overloaded.
const RATE_LIMITED_DELAY_MS: u32 = 5000;
/// `filetransfer-retry-max-delay`: the ceiling of the exponential backoff.
const MAX_DELAY_MS: u32 = 60_000;

/// A failed attempt, and whether Nix would try again (`Error::Transient`).
struct Failure {
    error: Error,
    transient: bool,
    /// The HTTP status, if the server answered.
    status: Option<u16>,
    /// `Retry-After`, in milliseconds.
    retry_after_ms: Option<u32>,
}

impl Failure {
    fn fatal(error: Error) -> Failure {
        Failure {
            error,
            transient: false,
            status: None,
            retry_after_ms: None,
        }
    }
}

/// A blocking HTTP client. One client serves the threads that fetch
/// inputs in parallel (Git fetches over HTTP included, see
/// [`super::git_http`]) and reports every transfer to its [`Progress`].
/// A clone shares the agent once it is made (a tree whose Git LFS files
/// are fetched when read keeps one).
#[derive(Clone)]
pub struct HttpClient {
    /// The CA certificates HTTPS transfers trust (`ssl-cert-file`), or
    /// none for brix's own bundle.
    ca_file: Option<PathBuf>,
    /// The agent, and why no HTTPS transfer can be made (the CA file is
    /// unusable; curl reports that for each transfer, as error 77), made
    /// for the first transfer. Reading and parsing a CA bundle costs more
    /// than a short evaluation does (lib.version through an image: 18M
    /// instructions of 51M), and most evaluations make no transfer; curl
    /// too reads its CA file only for a transfer.
    tls: OnceLock<(ureq::Agent, Option<String>)>,
    progress: Arc<dyn Progress>,
    /// Where the warnings about retried downloads go.
    warn: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Default for HttpClient {
    fn default() -> Self {
        HttpClient::new()
    }
}

/// The two bundles `FileTransferSettings::getDefaultSSLCertFile` looks
/// for, in order.
const DEFAULT_CA_FILES: [&str; 2] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt",
];

/// `ssl-cert-file`'s value before any configuration file is read
/// (`FileTransferSettings`' constructor): `NIX_SSL_CERT_FILE`, or else
/// `SSL_CERT_FILE` (an empty value is none, and an empty
/// `NIX_SSL_CERT_FILE` hides `SSL_CERT_FILE`), or else the first of
/// Nix's system bundles that exists.
pub fn default_ssl_cert_file() -> Option<PathBuf> {
    let from_env =
        std::env::var_os("NIX_SSL_CERT_FILE").or_else(|| std::env::var_os("SSL_CERT_FILE"));
    match from_env {
        Some(file) if !file.is_empty() => Some(PathBuf::from(file)),
        _ => DEFAULT_CA_FILES
            .iter()
            .map(PathBuf::from)
            .find(|file| file.exists()),
    }
}

impl HttpClient {
    /// A client trusting what Nix trusts without configuration:
    /// [`default_ssl_cert_file`].
    pub fn new() -> HttpClient {
        HttpClient::with_ca_file(default_ssl_cert_file().as_deref())
    }

    /// A client trusting the CA certificates in `ca_file` (Nix's
    /// `ssl-cert-file`), and only those, as curl does with `CAINFO`. A
    /// file that cannot be read or holds no certificate makes every HTTPS
    /// transfer fail as curl's do. Without a file, curl uses its default
    /// store, which in Nix as nixpkgs builds it is OpenSSL's: the file
    /// `NIX_SSL_CERT_FILE` or `SSL_CERT_FILE` names, else a bundle of its
    /// own; brix's own is Mozilla's (`webpki-roots`).
    pub fn with_ca_file(ca_file: Option<&Path>) -> HttpClient {
        let from_env = || {
            ["NIX_SSL_CERT_FILE", "SSL_CERT_FILE"]
                .iter()
                .filter_map(std::env::var_os)
                .find(|file| !file.is_empty())
                .map(PathBuf::from)
        };
        HttpClient {
            ca_file: ca_file.map(Path::to_path_buf).or_else(from_env),
            tls: OnceLock::new(),
            progress: Arc::new(NoProgress),
            warn: Arc::new(super::warn_to_stderr),
        }
    }

    /// The agent and the CA file's error, made on first use.
    fn tls(&self) -> &(ureq::Agent, Option<String>) {
        self.tls.get_or_init(|| {
            let (roots, ca_error) = match &self.ca_file {
                None => (RootCerts::WebPki, None),
                Some(file) => match load_ca_file(file) {
                    Some(certs) => (RootCerts::new_with_certs(&certs), None),
                    None => (
                        RootCerts::new_with_certs(&[]),
                        Some(format!(
                            "Problem with the SSL CA cert (path? access rights?) (77) error adding trust anchors from file: {}",
                            file.display()
                        )),
                    ),
                },
            };
            let config = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(10)
                .user_agent("brix")
                .tls_config(TlsConfig::builder().root_certs(roots).build())
                .build();
            (budget_agent(config, DefaultResolver::default()), ca_error)
        })
    }

    /// The agent requests go through (the Git transport's too).
    pub(crate) fn agent(&self) -> &ureq::Agent {
        &self.tls().0
    }

    /// curl's error for an HTTPS transfer with an unusable CA file, if
    /// `url` is one.
    pub(crate) fn ca_error(&self, url: &str) -> Option<&str> {
        let https = url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
        if !https {
            return None;
        }
        self.tls().1.as_deref()
    }

    /// The same client, reporting its transfers to `progress`.
    pub fn with_progress(mut self, progress: Arc<dyn Progress>) -> HttpClient {
        self.progress = progress;
        self
    }

    /// The same client, warning about retries through `warn`.
    pub fn with_warn(mut self, warn: Arc<dyn Fn(&str) + Send + Sync>) -> HttpClient {
        self.warn = warn;
        self
    }

    /// GET `url` with extra `headers`, following redirects. A `file://`
    /// URL is read from the file system. Credentials from
    /// `settings.netrc_file` are sent as basic authentication when the
    /// file has an entry for the URL's host and `headers` carry no
    /// `Authorization` of their own, as curl's optional netrc does for Nix.
    ///
    /// Like Nix's `FileTransfer`, a download that fails transiently (a
    /// connection problem, HTTP 408, 429 or most 5xx) is tried again after
    /// an exponentially growing delay, `settings.download_attempts` times
    /// in all (once for 0), with a warning before each retry. Each attempt
    /// may spend `settings.connect_timeout` seconds connecting.
    pub fn download(
        &self,
        url: &str,
        headers: &[(String, String)],
        settings: &FetchSettings,
    ) -> Result<Download> {
        if url.starts_with("file:") {
            let path = url_path_to_path(&parse_url(url, true, &*settings.warn)?.path)?;
            let data = std::fs::read(&path).map_err(|e| Error::Io {
                context: format!("reading '{path}'"),
                source: e,
            })?;
            return Ok(Download {
                data,
                immutable_url: None,
            });
        }
        self.with_retries(url, headers, None, settings)
    }

    /// POST `body` to `url` with extra `headers` (Nix's
    /// `FileTransfer::upload` of a request with `HttpMethod::Post`) and
    /// return the response, as [`HttpClient::download`] GETs: the same
    /// netrc credentials, retries and errors, which say `upload` where
    /// those say `download`. Only HTTP(S) URLs can be posted to.
    pub fn upload(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        settings: &FetchSettings,
    ) -> Result<Download> {
        // `FileTransfer::enqueueItem`: curl posts over HTTP(S) and S3
        // only, and brix has no S3.
        let scheme = url.split_once(':').map(|(scheme, _)| scheme);
        if !matches!(scheme, Some("http" | "https")) {
            return Err(err!("uploading to '{}' is not supported", url)
                .trace(format!("during upload of '{url}'")));
        }
        self.with_retries(url, headers, Some(body), settings)
    }

    /// Transfers `url` (a POST of `body`, if given, else a GET), trying a
    /// transient failure again as Nix's `TransferItem::maybeRetry` does.
    fn with_retries(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        settings: &FetchSettings,
    ) -> Result<Download> {
        let mut attempt = 0;
        loop {
            let failure = match self.attempt(url, headers, body, settings) {
                Ok(download) => return Ok(download),
                Err(failure) => failure,
            };
            attempt += 1;
            if !failure.transient || attempt >= settings.download_attempts {
                return Err(failure.error);
            }
            let base_ms = match failure.status {
                Some(429 | 503) => RATE_LIMITED_DELAY_MS,
                _ => settings.download_retry_delay_ms,
            };
            let delay_ms = retry_delay_ms(attempt, base_ms, failure.retry_after_ms);
            (self.warn)(&format!(
                "{}; retrying in {delay_ms} ms (attempt {attempt}/{})",
                failure.error, settings.download_attempts
            ));
            std::thread::sleep(Duration::from_millis(delay_ms.into()));
        }
    }

    /// One attempt at downloading `url`, or at posting `body` to it.
    fn attempt(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        settings: &FetchSettings,
    ) -> std::result::Result<Download, Failure> {
        // `FileTransferRequest::verb`.
        let verb = if body.is_some() { "upload" } else { "download" };
        // `CURLE_SSL_CACERT_BADFILE` is not transient.
        if let Some(e) = self.ca_error(url) {
            return Err(Failure::fatal(err!("unable to {} '{}': {}", verb, url, e)));
        }
        // curl's `t_startsingle`, from which it measures a timeout.
        let started = Instant::now();
        let sent = match body {
            None => self
                .prepare(self.agent().get(url), url, headers, settings)
                .call(),
            Some(body) => self
                .prepare(self.agent().post(url), url, headers, settings)
                .send(body),
        };
        let mut resp = sent.map_err(|e| Failure {
            transient: is_transient(&e),
            error: err!(
                "unable to {} '{}': {}",
                verb,
                url,
                failure_text(&e, started.elapsed())
            ),
            status: None,
            retry_after_ms: None,
        })?;
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let retry_after_ms = header("retry-after")
                .and_then(|value| parse_retry_after(&value, jiff::Timestamp::now()));
            let message = format!("unable to {verb} '{url}': HTTP error {status}");
            return Err(Failure {
                error: err!(
                    "{}",
                    with_response_body(message, &error_body(resp.body_mut()))
                ),
                transient: status_is_transient(status),
                status: Some(status),
                retry_after_ms,
            });
        }
        let immutable_url = header("link").and_then(|link| parse_immutable_link(&link));
        let data = self
            .read_body(url, verb, resp.body_mut())
            .map_err(|error| Failure {
                // The connection broke off: try again from the start.
                transient: true,
                ..Failure::fatal(error)
            })?;
        Ok(Download {
            data,
            immutable_url,
        })
    }

    /// `req` (a GET or a POST of `url`) with the connect timeout, the
    /// extra `headers` and the netrc credentials.
    fn prepare<B>(
        &self,
        mut req: ureq::RequestBuilder<B>,
        url: &str,
        headers: &[(String, String)],
        settings: &FetchSettings,
    ) -> ureq::RequestBuilder<B> {
        if let Some(timeout) = connect_timeout(settings) {
            // The lookup may take all of it; the agent's
            // [`BudgetConnector`] gives the connection what is left.
            req = req
                .config()
                .timeout_resolve(Some(timeout))
                .timeout_connect(Some(timeout))
                .build();
        }
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(auth) = netrc_authorization(url, headers, settings) {
            req = req.header("Authorization", auth.as_str());
        }
        req
    }

    /// Reads a response body, reporting the bytes received as Nix's
    /// `actFileTransfer` activity does: `downloading '…'` (`uploading
    /// '…'` for a POST, whose response this is), with progress against
    /// `Content-Length` when the server sends one.
    fn read_body(&self, url: &str, verb: &str, body: &mut ureq::Body) -> Result<Vec<u8>> {
        // A body decoded on the fly (`Content-Encoding: gzip`) yields more
        // bytes than its length announces; the estimate grows with it.
        let mut expected = body.content_length().unwrap_or(0);
        let activity = Activity::new(
            &*self.progress,
            ActivityKind::FileTransfer,
            &format!(
                "{} '{url}'",
                if verb == "upload" {
                    "uploading"
                } else {
                    "downloading"
                }
            ),
        );
        let mut reader = body.with_config().limit(u64::MAX).reader();
        // The announced length is only a hint for the first allocation.
        let mut data = Vec::with_capacity(expected.min(64 << 20) as usize);
        let mut chunk = vec![0u8; 256 * 1024];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(err!("unable to {} '{}': {}", verb, url, e)),
            };
            data.extend_from_slice(&chunk[..n]);
            let done = data.len() as u64;
            expected = expected.max(done);
            activity.update(done, expected);
        }
        Ok(data)
    }
}

/// The certificates in the PEM file `file`, or `None` when it cannot be
/// read or has none: what makes OpenSSL's `X509_load_cert_crl_file`, and
/// so curl, give up on a CA file.
fn load_ca_file(file: &Path) -> Option<Vec<Certificate<'static>>> {
    let pem = std::fs::read(file).ok()?;
    let mut certs = Vec::new();
    for item in ureq::tls::parse_pem(&pem) {
        if let PemItem::Certificate(cert) = item.ok()? {
            certs.push(cert);
        }
    }
    (!certs.is_empty()).then_some(certs)
}

/// The `rel="immutable"` target of a `Link` header, if any.
pub fn parse_immutable_link(link: &str) -> Option<String> {
    for part in link.split(',') {
        let part = part.trim();
        let (target, params) = part.split_once(';')?;
        let target = target.trim().strip_prefix('<')?.strip_suffix('>')?;
        if params.split(';').any(|p| {
            let p = p.trim();
            p == "rel=\"immutable\"" || p == "rel=immutable"
        }) {
            return Some(target.to_string());
        }
    }
    None
}

/// The `Authorization` value curl would add from the netrc file: only when
/// the request does not set one itself (an access token wins) and the file
/// has credentials for the URL's host.
fn netrc_authorization(
    url: &str,
    headers: &[(String, String)],
    settings: &FetchSettings,
) -> Option<String> {
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    {
        return None;
    }
    let path = settings.netrc_file.as_deref()?;
    let netrc = Netrc::load(path)?;
    // curl matches the host it parses itself; a URL it cannot parse has no
    // netrc entry, and parsing is not where Nix warns.
    let host = parse_url(url, true, &|_| {}).ok()?.authority?.host;
    netrc.basic_auth_header(&host)
}

/// `connect-timeout` as a duration; `None` for 0, which is no limit.
fn connect_timeout(settings: &FetchSettings) -> Option<Duration> {
    (settings.connect_timeout != 0).then_some(Duration::from_secs(settings.connect_timeout))
}

/// The agent requests go through, made from `config` and looking hosts
/// up with `resolver`, whose connections keep `connect-timeout` as one
/// budget, as curl keeps `CURLOPT_CONNECTTIMEOUT`: for looking the host
/// up, connecting and the TLS handshake together. ureq gives the lookup
/// (`timeout_resolve`) and the rest (`timeout_connect`) a budget each;
/// with both set to the timeout, [`BudgetResolver`] notes when the lookup
/// began and [`BudgetConnector`] leaves the connection only what remains.
fn budget_agent(config: ureq::config::Config, resolver: impl Resolver) -> ureq::Agent {
    ureq::Agent::with_parts(
        config,
        BudgetConnector(DefaultConnector::new()),
        BudgetResolver(resolver),
    )
}

thread_local! {
    /// When the connection this thread is making began: set by
    /// [`BudgetResolver`] as it looks the host up, taken by
    /// [`BudgetConnector`]. ureq looks up and connects on the thread that
    /// makes the request, one connection at a time, and every connection
    /// it makes starts with a lookup (or, through a proxy that looks the
    /// host up itself, with [`Resolver::empty`]), so the connector always
    /// finds its own connection's start here.
    static CONNECTION_STARTED: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// A resolver that marks the start of each connection's
/// `connect-timeout` budget (see [`budget_agent`]).
#[derive(Debug)]
struct BudgetResolver<R>(R);

impl<R: Resolver> Resolver for BudgetResolver<R> {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        CONNECTION_STARTED.set(Some(Instant::now()));
        self.0.resolve(uri, config, timeout)
    }

    /// A connection without a lookup of its host (a proxy looks it up).
    fn empty(&self) -> ResolvedSocketAddrs {
        CONNECTION_STARTED.set(Some(Instant::now()));
        self.0.empty()
    }
}

/// ureq's connectors, given what is left of the `connect-timeout` budget
/// the lookup started (see [`budget_agent`]). A request without
/// `timeout_connect` (Git's) connects as ureq would.
#[derive(Debug)]
struct BudgetConnector(DefaultConnector);

impl Connector for BudgetConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        let started = CONNECTION_STARTED.take();
        let (Some(started), Some(budget)) = (started, details.config.timeouts().connect) else {
            return self.0.connect(details, chained);
        };
        let left = budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            // The lookup took it all.
            return Err(ureq::Error::Timeout(ureq::Timeout::Connect));
        }
        if left >= *details.timeout.after {
            // A budget of the whole request runs out sooner.
            return self.0.connect(details, chained);
        }
        let details = ConnectionDetails {
            uri: details.uri,
            addrs: details.addrs.clone(),
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: details.now,
            timeout: NextTimeout {
                after: left.into(),
                reason: ureq::Timeout::Connect,
                ..details.timeout
            },
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
            register_shutdown: details.register_shutdown.clone(),
        };
        self.0.connect(&details, chained)
    }
}

/// A failure below HTTP as Nix's message shows it where users meet it
/// most: a server whose certificate no trusted CA signed (typically a
/// proxy that intercepts TLS, whose CA is not in `ssl-cert-file`), and a
/// connection that ran out of `connect-timeout` while looking the host up
/// or connecting (curl's `CURLE_OPERATION_TIMEDOUT`, saying how long the
/// transfer had taken: `elapsed`), get curl's text. Other failures keep
/// `ureq`'s.
fn failure_text(e: &ureq::Error, elapsed: Duration) -> String {
    // `multi_handle_timeout`, for the transfer's state.
    let timed_out = |phase: &str| {
        format!(
            "Timeout was reached (28) {phase} timed out after {} milliseconds",
            elapsed.as_millis()
        )
    };
    match e {
        ureq::Error::Io(io) if io.to_string() == "invalid peer certificate: UnknownIssuer" => {
            "SSL peer certificate or SSH remote key was not OK (60) SSL certificate OpenSSL verify result: unable to get local issuer certificate (20)".to_owned()
        }
        ureq::Error::Timeout(ureq::Timeout::Resolve) => timed_out("Resolving"),
        ureq::Error::Timeout(ureq::Timeout::Connect) => timed_out("Connection"),
        e => e.to_string(),
    }
}

/// What arrived of the body of an error response (curl's `errorSink`,
/// which holds what the server sent before the transfer ended): the
/// error being reported is the status, so a body cut off by a broken
/// connection is kept as far as it came rather than reported itself.
fn error_body(body: &mut ureq::Body) -> Vec<u8> {
    let mut reader = body.with_config().limit(u64::MAX).reader();
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    data
}

/// `FileTransferError`'s message: an error response's body, when there
/// is one, follows the message if it is shorter than 1024 bytes or is
/// HTML, without trailing white space (`chomp`).
fn with_response_body(message: String, body: &[u8]) -> String {
    if body.is_empty() {
        // curl's error sink is made by the first byte of the body.
        return message;
    }
    let text = String::from_utf8_lossy(body);
    if body.len() < 1024 || text.contains("<html>") {
        let text = text.trim_end_matches([' ', '\n', '\r', '\t']);
        format!("{message}\n\nresponse body:\n\n{text}")
    } else {
        message
    }
}

/// Whether Nix retries after an HTTP error status: not for 404 and 410
/// (not there), 401, 403 and 407 (not allowed), other 4xx but 408 and 429
/// (the request is wrong), 501, 505 and 511 (no use); yes for the rest.
fn status_is_transient(status: u16) -> bool {
    match status {
        408 | 429 => true,
        400..=499 => false,
        501 | 505 | 511 => false,
        _ => true,
    }
}

/// Whether Nix retries after a failure below HTTP: it treats most curl
/// errors as transient, but not a malformed URL, a protocol it cannot
/// speak, too many redirects, an unusable certificate file or a body it
/// cannot decode.
fn is_transient(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Protocol(_)
            | ureq::Error::Tls(_)
            | ureq::Error::Rustls(_)
            | ureq::Error::ConnectProxyFailed(_)
            | ureq::Error::RedirectFailed
            | ureq::Error::LargeResponseHeader(..)
            | ureq::Error::BodyStalled
            | ureq::Error::Other(_)
    )
}

/// A `Retry-After` header (RFC 7231 §7.1.3) as Nix's header callback
/// reads it: delay-seconds (`string2Int<uint32_t>`), or else an HTTP-date
/// (`curl_getdate`), whose distance from `now` is the delay (none for a
/// date in the past). In milliseconds, saturating; `None` for a value
/// that is neither, which Nix ignores.
fn parse_retry_after(value: &str, now: jiff::Timestamp) -> Option<u32> {
    let value = value.trim();
    let seconds = if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        u64::from(value.parse::<u32>().ok()?)
    } else {
        let date = parse_http_date(value)?;
        (date.as_second() - now.as_second()).max(0).unsigned_abs()
    };
    Some(u32::try_from(seconds.saturating_mul(1000)).unwrap_or(u32::MAX))
}

/// The three forms of HTTP-date (RFC 7231 §7.1.1.1): IMF-fixdate
/// (`Sun, 06 Nov 1994 08:49:37 GMT`), the obsolete RFC 850 form
/// (`Sunday, 06-Nov-94 08:49:37 GMT`) and asctime's
/// (`Sun Nov  6 08:49:37 1994`), all in UTC.
fn parse_http_date(value: &str) -> Option<jiff::Timestamp> {
    if let Ok(zoned) = jiff::fmt::rfc2822::parse(value) {
        return Some(zoned.timestamp());
    }
    ["%A, %d-%b-%y %H:%M:%S GMT", "%a %b %e %H:%M:%S %Y"]
        .into_iter()
        .find_map(|format| {
            let time = jiff::fmt::strtime::parse(format, value).ok()?;
            let utc = time.to_datetime().ok()?.to_zoned(jiff::tz::TimeZone::UTC);
            Some(utc.ok()?.timestamp())
        })
}

/// `computeRetryDelayMs` with full jitter: a random delay between the
/// server's `Retry-After` (if any) and that plus `base * 2^(attempt-1)`,
/// the exponential part capped at a minute.
fn retry_delay_ms(attempt: u32, base_ms: u32, retry_after_ms: Option<u32>) -> u32 {
    let shift = attempt.saturating_sub(1).min(31);
    let backoff = u32::try_from((u64::from(base_ms) << shift).min(u64::from(MAX_DELAY_MS)))
        .expect("invariant: capped at MAX_DELAY_MS");
    let floor = retry_after_ms.unwrap_or(0);
    let ceiling = floor.saturating_add(backoff);
    if ceiling <= floor {
        return floor;
    }
    floor + (random_u64() % u64::from(ceiling - floor + 1)) as u32
}

/// A random number, from the per-process random keys of the standard
/// library's hasher (good enough to spread retries apart).
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_link() {
        assert_eq!(
            parse_immutable_link("<https://x/y.tar.gz?rev=abc>; rel=\"immutable\"").as_deref(),
            Some("https://x/y.tar.gz?rev=abc")
        );
        assert_eq!(
            parse_immutable_link("<https://a>; rel=\"next\", <https://b>; rel=immutable")
                .as_deref(),
            Some("https://b")
        );
        assert_eq!(parse_immutable_link("<https://a>; rel=\"next\""), None);
    }

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// A server on localhost that answers its requests, one connection
    /// each, with `responses` in turn: (status, extra header lines, body).
    /// Returns its URL and the number of requests it has seen.
    fn serve(responses: Vec<(u16, &'static str, &'static str)>) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/file", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(0));
        let counter = seen.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                *counter.lock().unwrap() += 1;
                let mut stream = reader.into_inner();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, seen)
    }

    fn client() -> (HttpClient, Arc<Mutex<Vec<String>>>) {
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let sink = warnings.clone();
        let client = HttpClient::new().with_warn(Arc::new(move |w: &str| {
            sink.lock().unwrap().push(w.to_owned())
        }));
        (client, warnings)
    }

    fn quick_retries(attempts: u32) -> FetchSettings {
        FetchSettings {
            download_attempts: attempts,
            download_retry_delay_ms: 1,
            ..FetchSettings::default()
        }
    }

    #[test]
    fn transient_failures_are_retried() {
        let (url, seen) = serve(vec![(500, "", "no"), (502, "", "no"), (200, "", "ok")]);
        let (client, warnings) = client();
        let download = client.download(&url, &[], &quick_retries(5)).unwrap();
        assert_eq!(download.data, b"ok");
        assert_eq!(*seen.lock().unwrap(), 3);
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 2);
        // The error's whole message, the response's body included, as in
        // Nix's `warning: unable to download '…': HTTP error 500\n\nresponse
        // body:\n\nno; retrying in 60 ms (attempt 1/2)`.
        assert!(
            warnings[0].starts_with(&format!(
                "unable to download '{url}': HTTP error 500\n\nresponse body:\n\nno; retrying in "
            )) && warnings[0].ends_with(" ms (attempt 1/5)"),
            "{warnings:?}"
        );
        assert!(warnings[1].ends_with(" ms (attempt 2/5)"), "{warnings:?}");
    }

    #[test]
    fn hopeless_failures_are_not_retried() {
        for status in [404, 403, 400, 501] {
            let (url, seen) = serve(vec![(status, "", "no"), (200, "", "ok")]);
            let (client, warnings) = client();
            let err = client.download(&url, &[], &quick_retries(5)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("unable to download '{url}': HTTP error {status}\n\nresponse body:\n\nno")
            );
            assert_eq!(*seen.lock().unwrap(), 1);
            assert!(warnings.lock().unwrap().is_empty());
        }
    }

    /// `FileTransferError`: the body of an error response follows the
    /// message when there is one and it is short or HTML, without
    /// trailing white space (as `nix eval --expr 'builtins.fetchurl …'`
    /// shows against a server answering 404).
    #[test]
    fn error_responses_show_short_or_html_bodies() {
        let long_text: &'static str = "x".repeat(1024).leak();
        let long_html: &'static str = format!("<html>{}</html>", "x".repeat(1024)).leak();
        for (body, shown) in [
            ("not found\n\n", Some("not found")),
            ("", None),
            (long_text, None),
            (long_html, Some(long_html)),
        ] {
            let (url, _) = serve(vec![(404, "", body)]);
            let (client, _) = client();
            let err = client.download(&url, &[], &quick_retries(1)).unwrap_err();
            let message = format!("unable to download '{url}': HTTP error 404");
            let expected = match shown {
                Some(text) => format!("{message}\n\nresponse body:\n\n{text}"),
                None => message,
            };
            assert_eq!(err.to_string(), expected);
        }
    }

    /// Only HTTP(S) can be posted to (`FileTransfer::enqueueItem`).
    #[test]
    fn uploads_need_http() {
        let (client, _) = client();
        let err = client
            .upload("file:///tmp/x/objects/batch", &[], b"{}", &quick_retries(1))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "uploading to 'file:///tmp/x/objects/batch' is not supported"
        );
        assert_eq!(
            err.traces(),
            ["during upload of 'file:///tmp/x/objects/batch'"]
        );
    }

    #[test]
    fn attempts_are_limited() {
        let (url, seen) = serve(vec![(408, "", ""), (500, "", ""), (200, "", "ok")]);
        let (client, warnings) = client();
        let err = client.download(&url, &[], &quick_retries(2)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("unable to download '{url}': HTTP error 500")
        );
        assert_eq!(*seen.lock().unwrap(), 2);
        assert_eq!(warnings.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_refused_connection_is_retried() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/file");
        let (client, warnings) = client();
        client.download(&url, &[], &quick_retries(3)).unwrap_err();
        assert_eq!(warnings.lock().unwrap().len(), 2);
    }

    /// delay-seconds and the three HTTP-date forms; anything else is
    /// ignored.
    #[test]
    fn retry_after_takes_seconds_and_http_dates() {
        let now: jiff::Timestamp = "1994-11-06T08:48:37Z".parse().unwrap();
        assert_eq!(parse_retry_after("120", now), Some(120_000));
        assert_eq!(parse_retry_after(" 7 ", now), Some(7_000));
        assert_eq!(parse_retry_after("4294968", now), Some(u32::MAX));
        for date in [
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(parse_retry_after(date, now), Some(60_000), "{date}");
        }
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now),
            Some(0),
            "a date in the past is no delay"
        );
        for junk in ["", "+5", "-5", "soon", "4294967296"] {
            assert_eq!(parse_retry_after(junk, now), None, "{junk:?}");
        }
    }

    #[test]
    fn retry_delays_grow_and_respect_retry_after() {
        for attempt in 1..=4 {
            let backoff = 100 << (attempt - 1);
            for _ in 0..20 {
                assert!(retry_delay_ms(attempt, 100, None) <= backoff);
                let d = retry_delay_ms(attempt, 100, Some(1000));
                assert!((1000..=1000 + backoff).contains(&d), "{d}");
            }
        }
        assert!(retry_delay_ms(30, 5000, None) <= MAX_DELAY_MS);
    }

    /// HTTPS downloads trust the CA file they are given (`ssl-cert-file`),
    /// and only it: a certificate from another CA is refused (and, as in
    /// Nix, retried), and a CA file that cannot be used fails every
    /// HTTPS transfer, at once, with curl's error.
    #[test]
    fn https_trusts_the_ca_file() {
        use super::super::test_https::{fixture, serve};
        let url = format!("{}/file", serve(|_| (200, Vec::new(), b"secure".to_vec())));
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let client = |ca: &Path| {
            let sink = warnings.clone();
            HttpClient::with_ca_file(Some(ca)).with_warn(Arc::new(move |w: &str| {
                sink.lock().unwrap().push(w.to_owned())
            }))
        };
        let download = client(&fixture("ca.pem"))
            .download(&url, &[], &quick_retries(1))
            .unwrap();
        assert_eq!(download.data, b"secure");

        let err = client(&fixture("other-ca.pem"))
            .download(&url, &[], &quick_retries(2))
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("unable to download '{url}': ")),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            format!("unable to download '{url}': SSL peer certificate or SSH remote key was not OK (60) SSL certificate OpenSSL verify result: unable to get local issuer certificate (20)")
        );
        assert_eq!(
            warnings.lock().unwrap().len(),
            1,
            "an untrusted peer is retried"
        );
        warnings.lock().unwrap().clear();

        for bad in [Path::new("/nonexistent/ca.pem"), &fixture("server.key")] {
            let err = client(bad)
                .download(&url, &[], &quick_retries(5))
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "unable to download '{url}': Problem with the SSL CA cert (path? access rights?) (77) error adding trust anchors from file: {}",
                    bad.display()
                )
            );
        }
        assert!(
            warnings.lock().unwrap().is_empty(),
            "a bad CA file is not retried"
        );
        // Plain HTTP does not need it.
        let (plain, _) = serve_plain();
        let download = client(Path::new("/nonexistent/ca.pem"))
            .download(&plain, &[], &quick_retries(1))
            .unwrap();
        assert_eq!(download.data, b"ok");
    }

    fn serve_plain() -> (String, Arc<Mutex<usize>>) {
        serve(vec![(200, "", "ok")])
    }

    #[test]
    fn file_urls_are_percent_decoded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a b"), "local").unwrap();
        let url = format!("file://{}/a%20b", dir.path().display());
        let (client, _) = client();
        let download = client
            .download(&url, &[], &FetchSettings::default())
            .unwrap();
        assert_eq!(download.data, b"local");
    }

    /// `filetransfer-retry-attempts = 0`, which `nix --offline` sets: no
    /// retry, as with 1 (`maybeRetry` gives up once `attempt >= tries`).
    #[test]
    fn zero_attempts_try_once() {
        let (url, seen) = serve(vec![(500, "", ""), (200, "", "ok")]);
        let (client, warnings) = client();
        let err = client.download(&url, &[], &quick_retries(0)).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("unable to download '{url}': HTTP error 500")
        );
        assert_eq!(*seen.lock().unwrap(), 1);
        assert!(warnings.lock().unwrap().is_empty());
    }

    /// A listener that is never accepted: the kernel completes the TCP
    /// handshakes of its backlog, and nothing ever answers a client's TLS
    /// hello. Its `https://` URL; the listener must live as long as it is
    /// used.
    fn silent_https() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://{}/file", listener.local_addr().unwrap());
        (listener, url)
    }

    /// The milliseconds curl's message for a `phase` (`Resolving`,
    /// `Connection`) that ran out of time says the transfer took, which
    /// must be about `limit`.
    fn timed_out_after(error: &str, url: &str, phase: &str, limit: Duration) {
        let prefix = format!(
            "unable to download '{url}': Timeout was reached (28) {phase} timed out after "
        );
        let millis: u64 = error
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(" milliseconds"))
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("not curl's message for a {phase} timeout: {error}"));
        let took = Duration::from_millis(millis);
        assert!(
            took >= limit.mul_f32(0.95) && took < limit + Duration::from_millis(500),
            "{error}"
        );
    }

    /// A server that takes the TCP connection but never answers the TLS
    /// handshake: the attempt fails after `connect-timeout` seconds with
    /// curl's message, saying how long it took (`nix flake prefetch
    /// --connect-timeout 1 file+https://…` against such a server says
    /// "Timeout was reached (28) Connection timed out after 1000
    /// milliseconds"), and the failure is transient.
    #[test]
    fn a_connection_that_hangs_times_out() {
        let (listener, url) = silent_https();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let sink = warnings.clone();
        let ca = super::super::test_https::fixture("ca.pem");
        let client =
            HttpClient::with_ca_file(Some(ca.as_path())).with_warn(Arc::new(move |w: &str| {
                sink.lock().unwrap().push(w.to_owned())
            }));
        let settings = FetchSettings {
            connect_timeout: 1,
            ..quick_retries(2)
        };
        let start = std::time::Instant::now();
        let err = client.download(&url, &[], &settings).unwrap_err();
        let elapsed = start.elapsed();
        timed_out_after(&err.to_string(), &url, "Connection", Duration::from_secs(1));
        // Two attempts of a second each; Nix's default would be 15 s each.
        assert!(
            elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_secs(10),
            "{elapsed:?}"
        );
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 1, "a timeout is retried: {warnings:?}");
        drop(listener);
    }

    /// A name server that takes the given time to answer: within the lookup's budget
    /// it gives the address the default resolver finds, else the lookup
    /// fails when its budget runs out, as ureq's own does.
    #[derive(Debug)]
    struct SlowResolver(Duration);

    impl Resolver for SlowResolver {
        fn resolve(
            &self,
            uri: &ureq::http::Uri,
            config: &ureq::config::Config,
            timeout: NextTimeout,
        ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
            let budget = *timeout.after;
            if self.0 >= budget {
                std::thread::sleep(budget);
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            std::thread::sleep(self.0);
            DefaultResolver::default().resolve(uri, config, timeout)
        }
    }

    /// A client whose host lookups take `delay`.
    fn client_with_slow_lookups(delay: Duration) -> HttpClient {
        let client = HttpClient::with_ca_file(None);
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        assert!(
            client
                .tls
                .set((budget_agent(config, SlowResolver(delay)), None))
                .is_ok(),
            "the agent is made on first use"
        );
        client
    }

    /// `connect-timeout` is one budget, as curl's: a lookup that takes
    /// most of it leaves the connection only the rest, where ureq alone
    /// would give it the whole timeout again (1.8 s here, not 1 s).
    #[test]
    fn the_lookup_and_the_connection_share_the_timeout() {
        let (listener, url) = silent_https();
        let client = client_with_slow_lookups(Duration::from_millis(800));
        let settings = FetchSettings {
            connect_timeout: 1,
            ..quick_retries(1)
        };
        let start = std::time::Instant::now();
        let err = client.download(&url, &[], &settings).unwrap_err();
        let elapsed = start.elapsed();
        timed_out_after(&err.to_string(), &url, "Connection", Duration::from_secs(1));
        assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
        drop(listener);
    }

    /// A lookup that takes the whole budget fails as curl's does while
    /// resolving (`nix flake prefetch --connect-timeout 2
    /// --filetransfer-retry-attempts 1 file+https://<host>/x` with a name
    /// server that never answers: "Timeout was reached (28) Resolving
    /// timed out after 2000 milliseconds").
    #[test]
    fn a_lookup_that_hangs_times_out() {
        let (listener, url) = silent_https();
        let client = client_with_slow_lookups(Duration::from_secs(30));
        let settings = FetchSettings {
            connect_timeout: 1,
            ..quick_retries(1)
        };
        let start = std::time::Instant::now();
        let err = client.download(&url, &[], &settings).unwrap_err();
        let elapsed = start.elapsed();
        timed_out_after(&err.to_string(), &url, "Resolving", Duration::from_secs(1));
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        drop(listener);
    }

    /// A request that sets no connection timeout (Git's) is not limited.
    #[test]
    fn without_a_timeout_the_connector_does_not_limit() {
        let (url, seen) = serve(vec![(200, "", "ok")]);
        let client = client_with_slow_lookups(Duration::from_millis(50));
        let settings = FetchSettings {
            connect_timeout: 0,
            ..quick_retries(1)
        };
        let download = client.download(&url, &[], &settings).unwrap();
        assert_eq!(download.data, b"ok");
        assert_eq!(*seen.lock().unwrap(), 1);
    }

    #[test]
    fn connect_timeouts_in_seconds_zero_is_none() {
        let with = |secs| FetchSettings {
            connect_timeout: secs,
            ..FetchSettings::default()
        };
        assert_eq!(connect_timeout(&with(0)), None);
        assert_eq!(connect_timeout(&with(1)), Some(Duration::from_secs(1)));
        assert_eq!(
            connect_timeout(&FetchSettings::default()),
            Some(Duration::from_secs(15)),
            "Nix's default"
        );
    }
}
