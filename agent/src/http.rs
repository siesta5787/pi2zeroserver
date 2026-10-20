//! HTTPS downloads through the system libcurl (OpenSSL and the OS's CA
//! certificates). Blocking: call from `spawn_blocking`.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use blake2::Digest;

fn easy(url: &str) -> Result<curl::easy::Easy, String> {
    let mut e = curl::easy::Easy::new();
    let fail = |err: curl::Error| err.to_string();
    e.url(url).map_err(fail)?;
    e.follow_location(true).map_err(fail)?;
    e.fail_on_error(true).map_err(fail)?;
    e.connect_timeout(Duration::from_secs(30)).map_err(fail)?;
    // Give up if a download stalls below 1 KB/s for a minute
    e.low_speed_limit(1024).map_err(fail)?;
    e.low_speed_time(Duration::from_secs(60)).map_err(fail)?;
    Ok(e)
}

/// Fetches a small document into memory, refusing anything over `limit` bytes.
pub fn get(url: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut e = easy(url)?;
    let mut data = Vec::new();
    {
        let mut t = e.transfer();
        t.write_function(|chunk| {
            if data.len() + chunk.len() > limit {
                return Ok(0); // makes curl abort
            }
            data.extend_from_slice(chunk);
            Ok(chunk.len())
        })
        .map_err(|err| err.to_string())?;
        t.perform().map_err(|err| format!("{url}: {err}"))?;
    }
    Ok(data)
}

/// Downloads to `path` (at most `limit` bytes), hashing with `D` as it arrives.
/// Returns the hash and the length.
pub fn download<D: Digest>(
    url: &str,
    path: &Path,
    limit: u64,
    mut progress: impl FnMut(u8),
) -> Result<(Vec<u8>, u64), String> {
    let mut e = easy(url)?;
    e.progress(true).map_err(|err| err.to_string())?;
    let mut file = std::fs::File::create(path).map_err(|err| err.to_string())?;
    let mut hasher = D::new();
    let mut len = 0u64;
    let mut write_error = None;
    let result = {
        let mut t = e.transfer();
        t.write_function(|chunk| {
            if len + chunk.len() as u64 > limit {
                return Ok(0);
            }
            if let Err(err) = file.write_all(chunk) {
                write_error = Some(err.to_string());
                return Ok(0);
            }
            hasher.update(chunk);
            len += chunk.len() as u64;
            Ok(chunk.len())
        })
        .map_err(|err| err.to_string())?;
        t.progress_function(|total, now, _, _| {
            if total > 0.0 {
                progress((now / total * 100.0) as u8);
            }
            true
        })
        .map_err(|err| err.to_string())?;
        t.perform()
    };
    result.map_err(|err| write_error.unwrap_or_else(|| format!("{url}: {err}")))?;
    file.sync_all().map_err(|err| err.to_string())?;
    Ok((hasher.finalize().to_vec(), len))
}
