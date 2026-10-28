//! `upload`: publishes a packed archive as one object in S3-compatible storage (Cloudflare R2).
//! An existing object is accepted only if identical, the multipart upload is completed with
//! `If-None-Match: *` so it never replaces anything, failures and interrupts abort it, and the
//! object is read back and hashed at the end.
use crate::{
    pack::{self, PROGRESS_INTERVAL},
    s3::{self, S3Error},
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use clap::Args;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub const LONG_ABOUT: &str = "\
Upload a packed archive to S3-compatible object storage (Cloudflare R2) as one object.

Never overwrites. The key is checked first (HEAD): an existing object is accepted only if its size
and its sha256 user metadata (x-amz-meta-sha256) equal the archive's, and then nothing is
uploaded; any other existing object is refused. Otherwise the archive is sent as a multipart
upload with content type application/zstd and metadata sha256=<archive SHA-256> (and
bundle-format=<format> when the pack record names the bundle format, sourcify-arkiv.upload.v1 for
v3 or sourcify-arkiv.udc-bundle.v1 for v4; --expect-format refuses any other). Parts go in
parallel, each signed with its real SHA-256 (x-amz-content-sha256) so the server rejects
corrupted parts, and the upload is completed with If-None-Match: * so an object created meanwhile
is never replaced. Any failure, or SIGINT/SIGTERM, aborts the multipart upload. Finally the
object is read back and its size and SHA-256 are compared (skip with --no-verify-remote).

The archive SHA-256 comes from the `pack` record <FILE>.json when it matches the file's name and
size; the file is then hashed again while the parts upload, and the upload is completed only if
that matches. Without a matching record (or with --verify-local) the file is hashed first.

Credentials come from an env-style --credentials-file, else from the environment:
  AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY   key pair (never printed)
  AWS_ENDPOINT_URL_S3                       https://<account-id>.r2.cloudflarestorage.com
  AWS_DEFAULT_REGION                        optional, default auto
File lines are KEY=VALUE, optionally prefixed by `export `, with # comments and optional quotes
around values. Requests are path-style ({endpoint}/{bucket}/{key}), signed with AWS Signature V4.

Parts are --part-size-mib (at least 5) MiB, raised to whole MiB so that at most 9,000 parts are
needed. Memory use is about --concurrency x part size. Each request is tried up to 5 times with
exponential backoff on network errors, HTTP 5xx, 429 and RequestTimeout.

Progress goes to stderr as JSON lines; the result is one JSON line on stdout:
  {\"uri\":\"s3://B/K\",\"bytes\":N,\"sha256\":\"...\",\"bundleFormat\":\"...\"|null,\"uploaded\":true|false,
   \"verifiedRemote\":true|false}
uploaded is false when an identical object was already present.";

pub const EXAMPLES: &str = "\
Examples:
  # credentials file (chmod 600):
  #   AWS_ACCESS_KEY_ID=...
  #   AWS_SECRET_ACCESS_KEY=...
  #   AWS_ENDPOINT_URL_S3=https://<account-id>.r2.cloudflarestorage.com
  arkiv-entity-processor upload sourcify-2026-10-05.tar.zst --bucket arkiv-seeds \\
      --key sourcify/2026-10-05.tar.zst --credentials-file ~/.config/arkiv/r2.env";

/// Content type of uploaded archives.
pub const CONTENT_TYPE: &str = "application/zstd";
const MIB: u64 = 1 << 20;
/// Parts are sized so that at most this many are needed (S3 allows 10,000).
const TARGET_PARTS: u64 = 9_000;
/// S3 part size limits: 5 MiB (except the last part) to 5 GiB.
const MIN_PART_MIB: u64 = 5;
const MAX_PART_MIB: u64 = 5 * 1024;
/// Read-back request size; each range request has its own timeout and resumes on failure.
const VERIFY_RANGE: u64 = 64 * MIB;

#[derive(Args, Debug)]
pub struct UploadArgs {
    /// Archive written by `pack`
    #[arg(value_name = "FILE.tar.zst")]
    pub file: PathBuf,
    /// Destination bucket
    #[arg(long, value_name = "BUCKET")]
    pub bucket: String,
    /// Destination object key, e.g. sourcify/2026-10-05.tar.zst
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// Env-style credentials file [default: the environment]
    ///
    /// KEY=VALUE lines with AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_ENDPOINT_URL_S3 and
    /// optionally AWS_DEFAULT_REGION. Without it the same variables are read from the
    /// environment; when given, only the file is used.
    #[arg(long, value_name = "FILE")]
    pub credentials_file: Option<PathBuf>,
    /// Part size in MiB, at least 5
    ///
    /// Raised automatically so that at most 9,000 parts are needed.
    #[arg(long, value_name = "MIB", default_value_t = 64)]
    pub part_size_mib: u64,
    /// Parts uploaded in parallel, 1-64
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub concurrency: usize,
    /// Hash the archive before uploading even if its `pack` record matches
    #[arg(long)]
    pub verify_local: bool,
    /// Skip reading the object back after uploading
    #[arg(long)]
    pub no_verify_remote: bool,
    /// Refuse the upload unless the archive's `pack` record names this bundle format
    ///
    /// `v3` is sourcify-arkiv.upload.v1 (seed.json version 1), `udc` is
    /// sourcify-arkiv.udc-bundle.v1 (seed.json version 2).
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub expect_format: Option<ExpectFormat>,
}
/// A bundle format `upload --expect-format` can require.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectFormat {
    V3,
    Udc,
}
impl ExpectFormat {
    fn name(self) -> &'static str {
        match self {
            ExpectFormat::V3 => "sourcify-arkiv.upload.v1",
            ExpectFormat::Udc => arkiv_entity_processor::udc_bundle::FORMAT,
        }
    }
}

/// Part size in bytes: `part_size_mib` MiB, raised to whole MiB so that `size` needs at most
/// 9,000 parts; at most 5 GiB.
pub fn part_size(size: u64, part_size_mib: u64) -> Result<u64> {
    ensure!(
        (MIN_PART_MIB..=MAX_PART_MIB).contains(&part_size_mib),
        "part-size-mib must be between {MIN_PART_MIB} and {MAX_PART_MIB}"
    );
    let part = (part_size_mib * MIB)
        .max(size.div_ceil(TARGET_PARTS))
        .div_ceil(MIB)
        * MIB;
    ensure!(
        part <= MAX_PART_MIB * MIB,
        "a {size}-byte archive would need parts over 5 GiB"
    );
    Ok(part)
}

pub fn upload(a: &UploadArgs) -> Result<()> {
    ensure!(
        (1..=64).contains(&a.concurrency),
        "concurrency must be between 1 and 64"
    );
    s3::check_bucket(&a.bucket)?;
    s3::check_key(&a.key)?;
    let file = File::open(&a.file).with_context(|| format!("opening {}", a.file.display()))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "{} is not a regular file", a.file.display());
    let size = meta.len();
    ensure!(size > 0, "{} is empty", a.file.display());
    let part_size = part_size(size, a.part_size_mib)?;
    let credentials = s3::Credentials::load(a.credentials_file.as_deref())?;
    if credentials.endpoint().starts_with("http://") {
        eprintln!(
            "{}",
            json!({"warning": "AWS_ENDPOINT_URL_S3 uses plain http: data is not encrypted in transit"})
        );
    }
    // First SIGINT/SIGTERM: stop and abort the upload; a second one exits immediately.
    let stop = Arc::new(AtomicBool::new(false));
    for &signal in signal_hook::consts::TERM_SIGNALS {
        signal_hook::flag::register_conditional_shutdown(signal, 128 + signal, Arc::clone(&stop))?;
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    let (sha256, recheck, bundle_format) = archive_digest(a, size, &stop)?;
    if let Some(expected) = a.expect_format {
        ensure!(
            bundle_format.as_deref() == Some(expected.name()),
            "{} holds bundle format {}, not {}",
            a.file.display(),
            bundle_format
                .as_deref()
                .unwrap_or("unknown (no matching pack record)"),
            expected.name()
        );
    }
    let job = Job {
        client: s3::Client::new(credentials, a.concurrency),
        bucket: &a.bucket,
        key: &a.key,
        uri: format!("s3://{}/{}", a.bucket, a.key),
        path: &a.file,
        file,
        size,
        sha256,
        bundle_format,
        stop: &stop,
    };
    let uploaded = match job.head()? {
        Some(existing) => {
            job.accept_existing(&existing)?;
            eprintln!(
                "{}",
                json!({"phase": "present", "uri": job.uri, "note": "an identical object (size and sha256 metadata) already exists; nothing uploaded"})
            );
            false
        }
        None => job.multipart(part_size, a.concurrency, recheck)?,
    };
    let verified = !a.no_verify_remote;
    if verified {
        job.verify_remote()?;
    }
    println!(
        "{}",
        json!({"uri": job.uri, "bytes": size, "sha256": job.sha256, "bundleFormat": job.bundle_format, "uploaded": uploaded, "verifiedRemote": verified})
    );
    Ok(())
}

/// The archive SHA-256, whether it still has to be confirmed against the file, and the bundle
/// format: from a matching `pack` record (the digest confirmed while uploading), else hashed now
/// (format unknown).
fn archive_digest(
    a: &UploadArgs,
    size: u64,
    stop: &AtomicBool,
) -> Result<(String, bool, Option<String>)> {
    let name = a.file.file_name().and_then(|n| n.to_str());
    let record_path = pack::record_path(&a.file);
    let mut format = None;
    let recorded = match pack::read_record(&record_path) {
        Ok(r) if name.is_some_and(|n| r["archive"] == n) && r["bytes"].as_u64() == Some(size) => {
            format = r["bundleFormat"].as_str().map(String::from);
            r["sha256"].as_str().map(str::to_ascii_lowercase)
        }
        Ok(_) => {
            eprintln!(
                "{}",
                json!({"note": format!("{} does not match the archive name/size; hashing the archive", record_path.display())})
            );
            None
        }
        Err(e) if record_path.exists() => {
            eprintln!(
                "{}",
                json!({"note": format!("ignoring {}: {e:#}; hashing the archive", record_path.display())})
            );
            None
        }
        Err(_) => None,
    };
    match recorded {
        Some(sha256) if !a.verify_local => Ok((sha256, true, format)),
        recorded => {
            let (sha256, bytes) = pack::hash_file(&a.file, "hash-local", Some(stop))?;
            ensure!(bytes == size, "{} changed while hashing", a.file.display());
            if let Some(expected) = recorded {
                ensure!(
                    sha256 == expected,
                    "{} has SHA-256 {sha256}, but its record {} says {expected}",
                    a.file.display(),
                    record_path.display()
                );
            }
            Ok((sha256, false, format))
        }
    }
}

/// One archive going to one key.
struct Job<'a> {
    client: s3::Client,
    bucket: &'a str,
    key: &'a str,
    uri: String,
    path: &'a Path,
    file: File,
    size: u64,
    sha256: String,
    bundle_format: Option<String>,
    stop: &'a AtomicBool,
}

impl Job<'_> {
    fn head(&self) -> Result<Option<s3::ObjectInfo>> {
        s3::retry("HeadObject", Some(self.stop), || {
            self.client.head(self.bucket, self.key)
        })
        .map_err(|e| anyhow!("HEAD {}: {e}", self.uri))
    }

    /// An existing object is fine only if it is this archive.
    fn accept_existing(&self, existing: &s3::ObjectInfo) -> Result<()> {
        ensure!(
            existing.size == self.size && existing.sha256.as_deref() == Some(&self.sha256),
            "{} already exists ({} bytes, sha256 metadata {}) and is not this archive ({} bytes, sha256 {}); refusing to overwrite it",
            self.uri,
            existing.size,
            existing.sha256.as_deref().unwrap_or("missing"),
            self.size,
            self.sha256
        );
        // Same bytes, same bundle; an object stored without the metadata stays acceptable.
        if let (Some(theirs), Some(ours)) = (&existing.bundle_format, &self.bundle_format) {
            ensure!(
                theirs == ours,
                "{} is this archive but its bundle-format metadata says {theirs}, not {ours}",
                self.uri
            );
        }
        Ok(())
    }

    /// Multipart upload, aborted on any failure. Returns whether this run created the object.
    fn multipart(&self, part_size: u64, concurrency: usize, recheck: bool) -> Result<bool> {
        let upload_id = s3::retry("CreateMultipartUpload", Some(self.stop), || {
            self.client.create_multipart(
                self.bucket,
                self.key,
                CONTENT_TYPE,
                &self.sha256,
                self.bundle_format.as_deref(),
            )
        })
        .map_err(|e| anyhow!("starting the multipart upload of {}: {e}", self.uri))?;
        let result = self
            .upload_parts(&upload_id, part_size, concurrency, recheck)
            .and_then(|etags| self.complete(&upload_id, &etags));
        if result.is_err() {
            self.abort(&upload_id);
        }
        result
    }

    /// Best effort; reports the upload ID only if the abort fails.
    fn abort(&self, upload_id: &str) {
        match s3::retry("AbortMultipartUpload", None, || {
            self.client
                .abort_multipart(self.bucket, self.key, upload_id)
        }) {
            Ok(()) => eprintln!(
                "{}",
                json!({"phase": "aborted", "uri": self.uri, "note": "multipart upload aborted, its parts are discarded"})
            ),
            Err(e) if e.status().is_some_and(|(status, _)| status == 404) => eprintln!(
                "{}",
                json!({"phase": "aborted", "uri": self.uri, "note": format!("no multipart upload left to abort ({e})")})
            ),
            Err(e) => eprintln!(
                "{}",
                json!({"warning": format!("could not abort the multipart upload: {e}; abort it manually or let a lifecycle rule expire it"), "uri": self.uri, "uploadId": upload_id})
            ),
        }
    }

    /// Uploads all parts with `concurrency` threads, each reading its own byte range, and
    /// returns their ETags in part order. With `recheck` the whole file is hashed in parallel
    /// and must match the expected SHA-256.
    fn upload_parts(
        &self,
        upload_id: &str,
        part_size: u64,
        concurrency: usize,
        recheck: bool,
    ) -> Result<Vec<String>> {
        let parts = self.size.div_ceil(part_size);
        eprintln!(
            "{}",
            json!({"phase": "upload", "uri": self.uri, "bytes": self.size, "parts": parts, "partSize": part_size, "concurrency": concurrency})
        );
        // Set on the first failure or on a signal: workers stop taking parts.
        let cancel = AtomicBool::new(false);
        let next = AtomicU64::new(0);
        let done = (AtomicU64::new(0), AtomicU64::new(0));
        let etags = Mutex::new(vec![None; parts as usize]);
        let failure = Mutex::new(None);
        let start = Instant::now();
        let rehashed = thread::scope(|scope| {
            let (alive, finished) = mpsc::channel::<()>();
            for _ in 0..concurrency.min(parts as usize) {
                let alive = alive.clone();
                let (cancel, next, done, etags, failure) =
                    (&cancel, &next, &done, &etags, &failure);
                scope.spawn(move || {
                    let _alive = alive;
                    let mut buffer = Vec::new();
                    while !cancel.load(Ordering::Relaxed) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= parts {
                            break;
                        }
                        let offset = index * part_size;
                        let len = part_size.min(self.size - offset);
                        buffer.resize(len as usize, 0);
                        match self.upload_part(
                            upload_id,
                            index + 1,
                            parts,
                            offset,
                            &mut buffer,
                            cancel,
                        ) {
                            Ok(etag) => {
                                etags.lock().unwrap()[index as usize] = Some(etag);
                                done.0.fetch_add(1, Ordering::Relaxed);
                                done.1.fetch_add(len, Ordering::Relaxed);
                            }
                            Err(e) => {
                                failure.lock().unwrap().get_or_insert(e);
                                cancel.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
            drop(alive);
            let hasher = recheck.then(|| {
                scope.spawn(|| pack::hash_file(self.path, "recheck-local", Some(&cancel)))
            });
            let mut last = Instant::now();
            let mut interrupted = false;
            while let Err(mpsc::RecvTimeoutError::Timeout) =
                finished.recv_timeout(Duration::from_millis(200))
            {
                if self.stop.load(Ordering::Relaxed) && !interrupted {
                    interrupted = true;
                    cancel.store(true, Ordering::Relaxed);
                    eprintln!(
                        "{}",
                        json!({"phase": "interrupted", "note": "finishing parts in flight, then aborting the upload (signal again to exit at once)"})
                    );
                }
                if last.elapsed() >= PROGRESS_INTERVAL {
                    last = Instant::now();
                    let (parts_done, bytes) = (
                        done.0.load(Ordering::Relaxed),
                        done.1.load(Ordering::Relaxed),
                    );
                    let seconds = start.elapsed().as_secs_f64();
                    let rate = bytes as f64 / seconds;
                    eprintln!(
                        "{}",
                        json!({"phase": "upload", "parts": parts_done, "partsTotal": parts, "bytes": bytes, "totalBytes": self.size, "mibPerSecond": rate / MIB as f64, "etaSeconds": (rate > 0.).then(|| ((self.size - bytes) as f64 / rate).round()), "elapsedSeconds": seconds})
                    );
                }
            }
            // Workers are done (a failure already set `cancel`); a signal still stops the re-hash.
            hasher.map(|hasher| {
                while !hasher.is_finished() {
                    if self.stop.load(Ordering::Relaxed) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                hasher.join().expect("hash thread panicked")
            })
        });
        if self.stop.load(Ordering::Relaxed) {
            bail!("interrupted by signal");
        }
        if let Some(e) = failure.into_inner().unwrap() {
            return Err(e);
        }
        if let Some(rehashed) = rehashed {
            let (sha256, bytes) = rehashed?;
            ensure!(
                bytes == self.size && sha256 == self.sha256,
                "{} has SHA-256 {sha256} ({bytes} bytes) but its pack record says {}; not completing the upload",
                self.path.display(),
                self.sha256
            );
        }
        let seconds = start.elapsed().as_secs_f64();
        eprintln!(
            "{}",
            json!({"phase": "uploaded-parts", "parts": parts, "bytes": self.size, "seconds": seconds, "mibPerSecond": self.size as f64 / MIB as f64 / seconds})
        );
        etags
            .into_inner()
            .unwrap()
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .context("a part has no ETag")
    }

    /// Reads part `number` (1-based) into `buffer` and uploads it with its SHA-256.
    fn upload_part(
        &self,
        upload_id: &str,
        number: u64,
        parts: u64,
        offset: u64,
        buffer: &mut [u8],
        cancel: &AtomicBool,
    ) -> Result<String> {
        self.file
            .read_exact_at(buffer, offset)
            .with_context(|| format!("reading part {number} of {}", self.path.display()))?;
        let sha256 = hex::encode(Sha256::digest(&*buffer));
        s3::retry(&format!("UploadPart {number}"), Some(cancel), || {
            self.client
                .upload_part(self.bucket, self.key, upload_id, number, buffer, &sha256)
        })
        .map_err(|e| anyhow!("uploading part {number} of {parts}: {e}"))
    }

    /// CompleteMultipartUpload (If-None-Match: *). On 412, or NoSuchUpload after a lost
    /// response, the key is checked again: an identical object is accepted, anything else
    /// is refused.
    fn complete(&self, upload_id: &str, etags: &[String]) -> Result<bool> {
        // A failed attempt may still have completed the upload server-side.
        let mut maybe_completed = false;
        let result = s3::retry("CompleteMultipartUpload", Some(self.stop), || {
            let result = self
                .client
                .complete_multipart(self.bucket, self.key, upload_id, etags);
            maybe_completed |= matches!(&result, Err(e) if e.retryable());
            result
        });
        let error = match result {
            Ok(()) => return Ok(true),
            Err(e) => e,
        };
        let conflict = matches!(
            error.status(),
            Some((412, _) | (_, "PreconditionFailed") | (404, "NoSuchUpload"))
        );
        if !conflict {
            bail!("completing the multipart upload of {}: {error}", self.uri);
        }
        match self.head()? {
            Some(existing) if self.accept_existing(&existing).is_ok() => {
                if !maybe_completed {
                    self.abort(upload_id);
                }
                eprintln!(
                    "{}",
                    json!({"phase": "present", "uri": self.uri, "note": format!("completion answered {error}, but an identical object exists now")})
                );
                Ok(maybe_completed)
            }
            _ => bail!(
                "{} was created by someone else during the upload ({error}); refusing to overwrite it",
                self.uri
            ),
        }
    }

    /// Reads the object back in ranges (each resumable, pinned to the first response's ETag)
    /// and compares size and SHA-256.
    fn verify_remote(&self) -> Result<()> {
        let mut digest = Sha256::new();
        let mut offset = 0;
        let mut etag = None;
        let mut buffer = vec![0u8; 1 << 20];
        let (start, mut last) = (Instant::now(), Instant::now());
        while offset < self.size {
            let end = (offset + VERIFY_RANGE).min(self.size) - 1;
            s3::retry("GetObject", Some(self.stop), || {
                let response = self.client.get_range(
                    self.bucket,
                    self.key,
                    offset,
                    end,
                    etag.as_deref(),
                )?;
                let status = response.status().as_u16();
                let unexpected = |message: String| S3Error::Status {
                    status,
                    code: "UnexpectedResponse".into(),
                    message,
                };
                // A server ignoring Range answers 200 with the whole object.
                let last_byte = match status {
                    206 => {
                        let range = response
                            .headers()
                            .get("content-range")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default();
                        let expected = format!("bytes {offset}-{end}/{}", self.size);
                        if range != expected {
                            return Err(unexpected(format!(
                                "Content-Range {range:?}, expected {expected:?} (object size differs?)"
                            )));
                        }
                        end
                    }
                    200 if offset == 0 => self.size - 1,
                    status => return Err(unexpected(format!("status {status} for a range request"))),
                };
                if etag.is_none() {
                    etag = response
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                }
                let mut body = response.into_body().into_reader();
                while offset <= last_byte {
                    if self.stop.load(Ordering::Relaxed) {
                        return Err(S3Error::Interrupted);
                    }
                    let want = buffer.len().min((last_byte + 1 - offset) as usize);
                    let n = body
                        .read(&mut buffer[..want])
                        .map_err(|e| S3Error::Transport(format!("reading object: {e}")))?;
                    if n == 0 {
                        return Err(S3Error::Transport(format!(
                            "response ended at byte {offset} of {}",
                            last_byte + 1
                        )));
                    }
                    digest.update(&buffer[..n]);
                    offset += n as u64;
                    if last.elapsed() >= PROGRESS_INTERVAL {
                        last = Instant::now();
                        let seconds = start.elapsed().as_secs_f64();
                        let rate = offset as f64 / seconds;
                        eprintln!(
                            "{}",
                            json!({"phase": "verify-remote", "bytes": offset, "totalBytes": self.size, "mibPerSecond": rate / MIB as f64, "etaSeconds": ((self.size - offset) as f64 / rate).round(), "elapsedSeconds": seconds})
                        );
                    }
                }
                // Anything beyond the requested range means the object is larger.
                if body.read(&mut buffer[..1]).is_ok_and(|n| n > 0) {
                    return Err(unexpected("more data than requested".into()));
                }
                Ok(())
            })
            .map_err(|e| anyhow!("reading back {} at byte {offset}: {e}", self.uri))?;
        }
        let sha256 = hex::encode(digest.finalize());
        ensure!(
            sha256 == self.sha256,
            "{} reads back with SHA-256 {sha256}, expected {}: the stored object is not this archive",
            self.uri,
            self.sha256
        );
        eprintln!(
            "{}",
            json!({"phase": "verified-remote", "uri": self.uri, "bytes": offset, "sha256": sha256, "seconds": start.elapsed().as_secs_f64()})
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    #[test]
    fn part_sizes_stay_within_s3_limits() {
        assert_eq!(part_size(1, 64).unwrap(), 64 * MIB);
        assert_eq!(part_size(30 * MIB, 5).unwrap(), 5 * MIB);
        assert_eq!(part_size(9000 * 64 * MIB, 64).unwrap(), 64 * MIB);
        // 9,000 x 64 MiB + 1 byte: the next whole MiB.
        assert_eq!(part_size(9000 * 64 * MIB + 1, 64).unwrap(), 65 * MIB);
        for size in [200_000_000_000u64, 2_000_000_000_000, 4_999 * 1024 * MIB] {
            let part = part_size(size, 64).unwrap();
            assert_eq!(part % MIB, 0);
            assert!(size.div_ceil(part) <= TARGET_PARTS, "{size}");
        }
        assert!(part_size(1, 4).is_err());
        assert!(part_size(1, 5121).is_err());
        assert!(part_size(50 << 40, 64).is_err());
    }

    #[test]
    fn upload_cli_defaults() {
        let cli = Cli::try_parse_from([
            "arkiv-entity-processor",
            "upload",
            "a.tar.zst",
            "--bucket",
            "b",
            "--key",
            "k",
        ])
        .unwrap();
        let Command::Upload(a) = cli.command else {
            panic!("upload expected");
        };
        assert_eq!((a.part_size_mib, a.concurrency), (64, 8));
        assert!(a.credentials_file.is_none() && !a.verify_local && !a.no_verify_remote);
        assert!(Cli::try_parse_from(["arkiv-entity-processor", "upload", "a.tar.zst"]).is_err());
    }
}
