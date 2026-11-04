# gdrive patches to google-apis-common 8.0.0

This directory is google-apis-common 8.0.0 as published on crates.io (`.cargo_vcs_info.json`:
Byron/google-apis-rs commit `1f287eb9b222bbd9394a6e1b259e98598c100995`), used through
`[patch.crates-io]` in the top-level `Cargo.toml`. The version stays 8.0.0 so that google-drive3 7.0.0
keeps resolving to it. Only `src/lib.rs` is changed; every change is marked `GDRIVE PATCH` in the code.

Files: `src/`, `Cargo.toml`, `Cargo.toml.orig` and `.cargo_vcs_info.json` are copied unchanged from the
published crate. The crate does not ship a license file, so `LICENSE` is the MIT license of the
Byron/google-apis-rs repository (`LICENSE-MIT`), which is the license declared in `Cargo.toml`. The
published `CHANGELOG.md` and `Cargo.lock` are left out; they are not used to build.

## Status: gdrive no longer uploads through the patched helper

gdrive's own uploader (`src/transfer/upload.rs`, milestone 2 item 1 of docs/triage/README.md) now makes
every media upload: `files upload`, `files update`, `files import` and the files of
`files upload --recursive` (through `files::upload::upload_file`). It speaks the resumable protocol
directly over the hub's HTTP client, so `ResumableUploadHelper` and the multipart upload of
google-drive3's `upload()` are no longer called for file content, and gdrive has no
google-apis-common `Delegate` for uploads any more (the `UploadDelegate` of `src/common/delegate.rs`
is gone). google-drive3 7 is still used for every metadata call (and for the empty multipart body of
`files mkdir`), and it still links the helper, so the patch stays: dropping it would bring the bugs
below back for any code that calls `upload_resumable` again. `tests/resumable_upload.rs` keeps
testing the patched helper with a minimal delegate; the same scenarios against gdrive's own uploader
are in `src/transfer/upload/tests.rs`. The vendored copy can be dropped when nothing in gdrive calls
google-drive3's upload methods.

## Why

google-drive3 7 performs resumable uploads (gdrive's `files upload` for files larger than the chunk
size, 32 MiB by default) with `ResumableUploadHelper` from this crate. gdrive 3.9.1 built it from the
prasmussen/google-apis-rs `resumable-fix` branch, whose commits 9d336ab "Retry previous chunk" and
1f75444 "Retry if reading body fails" fixed two bugs that 8.0.0 still has (Byron/google-apis-rs#438,
open since 2023; analysis in docs/triage/report-fork-dodomorandi.md section 3):

1. The offset advanced as soon as any reply arrived. When the delegate asked to retry a chunk after a
   5xx or 429, the next request sent the *following* chunk, Google rejected it ("upload offset ...
   exceeds already uploaded size") and the upload stalled until the retry budget ran out.
2. A reply whose body could not be read became an empty body. For the reply that completes the upload,
   google-drive3 then failed to decode the file resource and gdrive reported an error although the
   file had been created, so a re-run created a duplicate.

## Changes

Checked against Google's protocol description,
<https://developers.google.com/workspace/drive/api/guides/manage-uploads>.

1. **Retried chunk is resent** (`upload`). `start` no longer advances on every reply, only on a 308, so
   a chunk retried after a failure is sent again from its first byte. Same effect as the
   `start -= request_size` of the prasmussen fork.
2. **Lost reply after success** (`upload`). If the body of a 2xx reply cannot be read, the helper asks
   the session for its status (an empty PUT with `Content-Range: bytes */TOTAL`, Google's documented
   step after an interrupted request). A finished upload answers with 200 or 201 and the file
   resource, which becomes the result. The prasmussen fork resent the last chunk instead; what Google
   does with more data for a finished session is not documented, so the status request is used. At
   most three lost replies are followed up per upload, then the old behavior (empty body) applies.
3. **308 Range header** (`RangeResponseHeader::from_bytes`, new `next_offset_after_308`). Google sends
   `Range: bytes=0-42`; the parser only accepted `bytes 0-42` and panicked on anything else. It now
   accepts both and returns `None` instead of panicking. After a 308 the next offset is the last byte
   of the Range plus one, or 0 when the reply has no Range header ("no bytes have been received"). The
   chunk loop now follows that offset ("Don't assume that the server received all bytes sent in the
   previous request") and falls back to the old assumption, the end of the chunk, only for a Range it
   cannot parse or that lies past the end of the file. `query_transfer_status` returned the last byte
   itself instead of the next offset; fixed as well.
4. **Status request results** (`query_transfer_status`). A 200 or 201 reply to the status request is
   returned as the result instead of being handed to the delegate as a failure, and a lost body is
   asked for again (at most three times, one second apart).
5. **PUT** for chunk and status requests, the method Google documents for both (the crate used POST,
   which Google also accepts today).
6. **308 without progress** (`upload`). Following the 308 Range (change 3) means a server that keeps
   answering with the same Range, or with no Range while the chunk starts at 0, would get the same
   chunk resent at once, forever, without the delegate being asked. A 308 whose next offset is not
   past the start of the chunk just sent now counts as a failed attempt: it goes to the delegate's
   `http_failure`, whose `Retry::After` backoff applies before the resend, and `Retry::Abort` returns
   the 308 as the result (google-drive3 reports it as a failure). gdrive's `UploadDelegate`
   (`src/common/delegate.rs`, removed since) retried such a 308 like a 5xx, within the same retry
   budget.

Not changed: after a 5xx or 429 the chunk is resent as in gdrive 3.9.1, without a status request first;
transport errors (no reply at all) were already retried with the same chunk; seeks and reads of the
local file still `unwrap()`. gdrive's own uploader (milestone 2 in docs/triage/README.md) has replaced
this helper for gdrive's uploads (see the status above), and the patch should be offered upstream for
#438.

## Tests

`tests/resumable_upload.rs` runs a `files.create` resumable upload through the helper, with a minimal
delegate that retries a failed chunk a few milliseconds later (as gdrive's `UploadDelegate` did),
against an in-process mock of the protocol: a 503 on the second chunk, a cut reply body on the final
chunk, and a 308 whose Range covers only part of a chunk. All three fail with pristine 8.0.0 and pass
with this patch. A fourth case answers every attempt at one chunk with the same 308 Range and checks
that the upload fails after a bounded number of attempts; without change 6 it never finishes. (Until
gdrive had its own uploader these tests ran gdrive's delegate, and a fifth case checked its
`Retry-After` and shared throttle; that case now runs against the own uploader.)

## Updating

To see the patch, or to reapply it to a newer release:

    diff -u ~/.cargo/registry/src/index.crates.io-*/google-apis-common-8.0.0/src/lib.rs \
        vendor/google-apis-common/src/lib.rs

The vendored copy can be dropped once a crates.io release fixes the bugs above and the tests still
pass against it.

## Diff against pristine 8.0.0

```diff
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -560,22 +560,32 @@
 pub struct RangeResponseHeader(pub Chunk);
 
 impl RangeResponseHeader {
-    fn from_bytes(raw: &[u8]) -> Self {
-        if !raw.is_empty() {
-            if let Ok(s) = std::str::from_utf8(raw) {
-                const PREFIX: &str = "bytes ";
-                if let Some(stripped) = s.strip_prefix(PREFIX) {
-                    if let Ok(c) = <Chunk as FromStr>::from_str(stripped) {
-                        return RangeResponseHeader(c);
-                    }
-                }
-            }
-        }
-
-        panic!("Unable to parse Range header {raw:?}")
+    // GDRIVE PATCH: Google sends `Range: bytes=0-42` (the original only accepted `bytes 0-42`)
+    // and an unparseable header is reported to the caller instead of panicking.
+    fn from_bytes(raw: &[u8]) -> Option<Self> {
+        let s = std::str::from_utf8(raw).ok()?;
+        let range = s.strip_prefix("bytes=").or_else(|| s.strip_prefix("bytes "))?;
+        <Chunk as FromStr>::from_str(range.trim())
+            .ok()
+            .map(RangeResponseHeader)
     }
 }
 
+// GDRIVE PATCH: the offset to send next after a 308 Resume Incomplete reply. Its `Range` header
+// lists the bytes the server holds (`bytes=0-42` means resume at 43); without one, the server
+// holds no bytes. Returns `None` for a header that cannot be parsed or lies past the end.
+fn next_offset_after_308(headers: &HeaderMap, content_length: u64) -> Option<u64> {
+    let offset = match headers.get("Range") {
+        None => 0,
+        Some(value) => RangeResponseHeader::from_bytes(value.as_bytes())?.0.last + 1,
+    };
+    (offset <= content_length).then_some(offset)
+}
+
+// GDRIVE PATCH: how often a reply whose body cannot be read is followed up before giving up.
+const MAX_LOST_REPLIES: u32 = 3;
+const LOST_REPLY_DELAY: Duration = Duration::from_secs(1);
+
 /// A utility type to perform a resumable upload from start to end.
 pub struct ResumableUploadHelper<'a, A: 'a, C>
 where
@@ -601,12 +611,14 @@
         &mut self,
     ) -> std::result::Result<u64, std::result::Result<Response, hyper_util::client::legacy::Error>>
     {
+        let mut lost_replies = 0;
         loop {
             match self
                 .client
                 .request(
                     hyper::Request::builder()
-                        .method(hyper::Method::POST)
+                        // GDRIVE PATCH: the documented method for status requests is PUT.
+                        .method(hyper::Method::PUT)
                         .uri(self.url)
                         .header(USER_AGENT, self.user_agent.to_string())
                         .header(
@@ -625,23 +637,36 @@
             {
                 Ok(r) => {
                     // 308 = resume-incomplete == PermanentRedirect
-                    let headers = r.headers().clone();
-                    let h: RangeResponseHeader = match headers.get("Range") {
-                        Some(hh) if r.status() == StatusCode::PERMANENT_REDIRECT => {
-                            RangeResponseHeader::from_bytes(hh.as_bytes())
+                    // GDRIVE PATCH: resume after the last byte the server holds (the original
+                    // returned the last byte itself), at 0 when the reply has no Range header.
+                    if r.status() == StatusCode::PERMANENT_REDIRECT {
+                        if let Some(offset) = next_offset_after_308(r.headers(), self.content_length)
+                        {
+                            return Ok(offset);
                         }
-                        None | Some(_) => {
-                            let (parts, body) = r.into_parts();
-                            let body = to_body(to_bytes(body).await);
-                            let response = Response::from_parts(parts, body);
-                            if let Retry::After(d) = self.delegate.http_failure(&response, None) {
-                                sleep(d).await;
-                                continue;
-                            }
-                            return Err(Ok(response));
+                    }
+                    let (parts, body) = r.into_parts();
+                    // GDRIVE PATCH: ask again when the reply body is lost.
+                    let bytes = match to_bytes(body).await {
+                        Some(bytes) => bytes,
+                        None if lost_replies < MAX_LOST_REPLIES => {
+                            lost_replies += 1;
+                            sleep(LOST_REPLY_DELAY).await;
+                            continue;
                         }
+                        None => Default::default(),
                     };
-                    return Ok(h.0.last);
+                    let response = to_response(parts, bytes.into());
+                    // GDRIVE PATCH: a finished upload answers with 200 or 201 and the resource;
+                    // that is the result, not a failure to hand to the delegate.
+                    if response.status().is_success() {
+                        return Err(Ok(response));
+                    }
+                    if let Retry::After(d) = self.delegate.http_failure(&response, None) {
+                        sleep(d).await;
+                        continue;
+                    }
+                    return Err(Ok(response));
                 }
                 Err(err) => {
                     if let Retry::After(d) = self.delegate.http_error(&err) {
@@ -674,6 +699,7 @@
             _ => MIN_CHUNK_SIZE,
         };
 
+        let mut lost_replies = 0;
         loop {
             self.reader.seek(SeekFrom::Start(start)).unwrap();
 
@@ -700,7 +726,8 @@
                 .request(
                     hyper::Request::builder()
                         .uri(self.url)
-                        .method(hyper::Method::POST)
+                        // GDRIVE PATCH: the documented method for chunk requests is PUT.
+                        .method(hyper::Method::PUT)
                         .header("Content-Range", range_header.header_value())
                         .header(CONTENT_TYPE, format!("{}", self.media_type))
                         .header(USER_AGENT, self.user_agent.to_string())
@@ -710,15 +737,52 @@
                 .await
             {
                 Ok(response) => {
-                    start += request_size;
-
+                    // GDRIVE PATCH: `start` only moves on a 308, and then to where the server's
+                    // Range header says it is (the server may hold less than the whole chunk).
+                    // The original advanced `start` on every reply, so a chunk retried after a
+                    // 5xx or 429 was skipped and the session got stuck.
                     if response.status() == StatusCode::PERMANENT_REDIRECT {
+                        let next = next_offset_after_308(response.headers(), self.content_length)
+                            .unwrap_or(start + request_size);
+                        // GDRIVE PATCH: a 308 that does not get past the start of this chunk
+                        // (the same Range again, or no Range at offset 0) made no progress.
+                        // Resending at once could loop forever, so it counts as a failed
+                        // attempt: the delegate decides, and its backoff applies.
+                        if next <= start {
+                            let (parts, body) = response.into_parts();
+                            let bytes = to_bytes(body).await.unwrap_or_default();
+                            let response = to_response(parts, bytes.into());
+                            if let Retry::After(d) = self.delegate.http_failure(&response, None) {
+                                sleep(d).await;
+                                start = next;
+                                continue;
+                            }
+                            return Some(Ok(response));
+                        }
+                        start = next;
                         continue;
                     }
 
                     let (parts, body) = response.into_parts();
                     let success = parts.status.is_success();
-                    let bytes = to_bytes(body).await.unwrap_or_default();
+                    // GDRIVE PATCH: when the body of a success reply is lost, the upload most
+                    // likely completed. Ask the session for its status, as Google documents for
+                    // an interrupted request, instead of reporting an empty body that fails to
+                    // decode (and makes a re-run create a duplicate file).
+                    let bytes = match to_bytes(body).await {
+                        Some(bytes) => bytes,
+                        None if success && lost_replies < MAX_LOST_REPLIES => {
+                            lost_replies += 1;
+                            match self.query_transfer_status().await {
+                                Ok(offset) => {
+                                    start = offset;
+                                    continue;
+                                }
+                                Err(result) => return Some(result),
+                            }
+                        }
+                        None => Default::default(),
+                    };
                     let error = if !success {
                         serde_json::from_str(&to_string(&bytes)).ok()
                     } else {
@@ -731,6 +795,7 @@
                             self.delegate.http_failure(&response, error.as_ref())
                         {
                             sleep(d).await;
+                            // GDRIVE PATCH: `start` still points at this chunk: resend it.
                             continue;
                         }
                     }
```
