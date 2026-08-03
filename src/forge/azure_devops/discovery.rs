//! Caching for Azure DevOps API resource-location discovery.
//!
//! Discovery is the `az devops invoke` form with no `--area`/`--resource`. It
//! makes the CLI enumerate the organization's entire API surface, which was
//! measured at 16-22s per call against a live organization, while a *targeted*
//! `az devops invoke --area git --resource <name>` call takes ~1s.
//!
//! [`super::AzureDevOpsBackend::invoke`] needs one discovery lookup per REST
//! call, so an uncached PR open paid that cost three times (commits, review
//! metadata, review threads) and dominated the load time. The resource-location
//! table describes the organization's API surface, not any pull request, so it
//! is safe to memoize in-process and to persist across runs behind a TTL.
//!
//! Two layers:
//!
//! 1. A process-wide memo, so the background threads spawned per PR open share
//!    a single fetch instead of racing three of them.
//! 2. An on-disk JSON document under the user cache directory with a 7-day TTL,
//!    so subsequent runs skip discovery entirely.
//!
//! A route lookup that misses inside a cached table triggers exactly one forced
//! refetch (see the caller), so a stale entry can never permanently hide a route
//! that Azure has since added.
//!
//! Caching is **disabled under `cfg(test)`** unless a [`TestCache`] guard is
//! held. The Azure unit tests script an exact queue of `az` responses, and a
//! cache shared across tests would make those queues order-dependent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;

use super::models::{AzureApiResourceLocation, parse_azure_api_resources};

/// Bump when the on-disk shape changes so old files are ignored, not misread.
const CACHE_FORMAT_VERSION: u32 = 1;
const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Relocates the cache directory. Set it to an empty value to turn the disk
/// layer off; the in-process memo still applies.
const CACHE_DIR_ENV: &str = "TUICR_AZ_DISCOVERY_CACHE_DIR";
/// Overrides the disk TTL in seconds. `0` treats every entry as stale.
const CACHE_TTL_ENV: &str = "TUICR_AZ_DISCOVERY_TTL_SECS";

pub(crate) type ResourceTable = Arc<Vec<AzureApiResourceLocation>>;

/// A resource table plus whether it came from a cache layer rather than a live
/// `az` call. The caller needs that distinction: retrying a failed route lookup
/// is only worthwhile against a cached table, and a blind retry would otherwise
/// double the `az` calls on a genuinely missing route.
pub(crate) struct LookupResult {
    pub(crate) table: ResourceTable,
    pub(crate) from_cache: bool,
}

static MEMO: LazyLock<Mutex<HashMap<String, ResourceTable>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-organization fetch locks. The PR open path issues its independent forge
/// calls concurrently, so without this a cold cache would let every one of them
/// start its own 16-22s discovery instead of one fetch serving all of them.
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    organization_url: String,
    fetched_at_unix: u64,
    resources: Vec<AzureApiResourceLocation>,
}

/// Return the resource-location table for `organization_url`.
///
/// `fetch` produces the raw discovery JSON and runs only on a miss. When
/// `force_refresh` is set both layers are bypassed and then refreshed.
pub(crate) fn resource_table<F>(
    organization_url: &str,
    force_refresh: bool,
    fetch: F,
) -> Result<LookupResult>
where
    F: FnOnce() -> Result<String>,
{
    if !caching_enabled() {
        return Ok(LookupResult {
            table: Arc::new(parse_azure_api_resources(&fetch()?)?),
            from_cache: false,
        });
    }

    if force_refresh {
        // Drop the entry first so the post-lock recheck below cannot hand back
        // the very table the caller just rejected.
        invalidate(organization_url);
    } else if let Some(hit) = lookup(organization_url) {
        return Ok(hit);
    }

    // Serialize fetches per organization. The lock is held across `fetch` on
    // purpose: waiters should block on the one in-flight discovery and then read
    // its result, not launch their own.
    let fetching = fetch_lock(organization_url);
    let _fetching = lock(&fetching);
    if let Some(hit) = lookup(organization_url) {
        return Ok(hit);
    }

    let table = Arc::new(parse_azure_api_resources(&fetch()?)?);
    memo_put(organization_url, &table);
    write_disk(organization_url, &table);
    Ok(LookupResult {
        table,
        from_cache: false,
    })
}

/// Read through the memo, then the disk layer, promoting a disk hit into the memo.
fn lookup(organization_url: &str) -> Option<LookupResult> {
    if let Some(table) = memo_get(organization_url) {
        return Some(LookupResult {
            table,
            from_cache: true,
        });
    }
    let table = Arc::new(read_disk(organization_url)?);
    memo_put(organization_url, &table);
    Some(LookupResult {
        table,
        from_cache: true,
    })
}

fn invalidate(organization_url: &str) {
    lock(&MEMO).remove(organization_url);
    if let Some(path) = cache_path(organization_url) {
        let _ = std::fs::remove_file(path);
    }
}

/// The map guard is released before the returned lock is taken, so a long fetch
/// for one organization never blocks lookups for another.
fn fetch_lock(organization_url: &str) -> Arc<Mutex<()>> {
    Arc::clone(
        lock(&IN_FLIGHT)
            .entry(organization_url.to_string())
            .or_default(),
    )
}

fn memo_get(organization_url: &str) -> Option<ResourceTable> {
    lock(&MEMO).get(organization_url).cloned()
}

fn memo_put(organization_url: &str, table: &ResourceTable) {
    lock(&MEMO).insert(organization_url.to_string(), Arc::clone(table));
}

/// Take a lock, tolerating poisoning. A panic elsewhere must not turn this
/// best-effort cache into a hard failure for every later Azure call.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn ttl_secs() -> u64 {
    #[cfg(test)]
    if let Some(ttl) = TEST_STATE.with(|state| state.borrow().ttl) {
        return ttl;
    }

    std::env::var(CACHE_TTL_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

/// Cache directory, or `None` when the disk layer is unavailable or disabled.
fn cache_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(directory) = TEST_STATE.with(|state| state.borrow().directory.clone()) {
        return Some(directory);
    }

    if let Some(value) = std::env::var_os(CACHE_DIR_ENV) {
        if value.is_empty() {
            return None;
        }
        return Some(PathBuf::from(value));
    }

    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .filter(|value| !value.is_empty())?;
        Some(PathBuf::from(base).join("tuicr").join("azure-discovery"))
    }

    #[cfg(not(windows))]
    {
        let base = match std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => PathBuf::from(std::env::var_os("HOME").filter(|value| !value.is_empty())?)
                .join(".cache"),
        };
        Some(base.join("tuicr").join("azure-discovery"))
    }
}

/// Filename for an organization URL: a readable slug for humans browsing the
/// cache directory, plus a hash so organizations differing only in punctuation
/// cannot collide.
fn cache_file_name(organization_url: &str) -> String {
    let mut slug = String::with_capacity(organization_url.len());
    for character in organization_url.chars() {
        if character.is_ascii_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(80).collect();
    format!("{slug}-{:016x}.json", fnv1a64(organization_url))
}

fn fnv1a64(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn cache_path(organization_url: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(cache_file_name(organization_url)))
}

/// Read a fresh, matching entry. Every failure yields `None` so the caller falls
/// back to a live fetch; a corrupt cache must never surface as an error.
fn read_disk(organization_url: &str) -> Option<Vec<AzureApiResourceLocation>> {
    let raw = std::fs::read_to_string(cache_path(organization_url)?).ok()?;
    let cached: CacheFile = serde_json::from_str(&raw).ok()?;
    if cached.version != CACHE_FORMAT_VERSION
        || cached.organization_url != organization_url
        || cached.resources.is_empty()
    {
        return None;
    }
    // A TTL of zero disables reuse outright, so it must not be satisfied by a
    // zero-second age.
    let ttl = ttl_secs();
    if ttl == 0 || now_unix().saturating_sub(cached.fetched_at_unix) > ttl {
        return None;
    }
    Some(cached.resources)
}

/// Best-effort persist. Writes a process-unique temp file then renames, so a
/// concurrent reader never observes a half-written document.
fn write_disk(organization_url: &str, table: &ResourceTable) {
    if table.is_empty() {
        return;
    }
    let Some(path) = cache_path(organization_url) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(serialized) = serde_json::to_string(&CacheFile {
        version: CACHE_FORMAT_VERSION,
        organization_url: organization_url.to_string(),
        fetched_at_unix: now_unix(),
        resources: table.as_ref().clone(),
    }) else {
        return;
    };
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&temporary, serialized).is_err()
        || std::fs::rename(&temporary, &path).is_err()
    {
        let _ = std::fs::remove_file(&temporary);
    }
}

#[cfg(not(test))]
fn caching_enabled() -> bool {
    true
}

#[cfg(test)]
fn caching_enabled() -> bool {
    TEST_STATE.with(|state| state.borrow().enabled)
}

/// Test overrides are **thread-local**: `cargo test` runs each test on its own
/// thread, and a global toggle would switch caching on for unrelated Azure tests
/// running in parallel, breaking their scripted `az` response queues.
#[cfg(test)]
#[derive(Default)]
struct TestState {
    enabled: bool,
    directory: Option<PathBuf>,
    ttl: Option<u64>,
}

#[cfg(test)]
thread_local! {
    static TEST_STATE: std::cell::RefCell<TestState> =
        std::cell::RefCell::new(TestState::default());
}

/// Serializes tests that share the process-wide [`MEMO`].
#[cfg(test)]
static TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Enables caching into a throwaway directory for the guard's lifetime.
///
/// Also held by [`super::tests`] to prove `invoke` discovers only once.
#[cfg(test)]
pub(crate) struct TestCache {
    _serial: std::sync::MutexGuard<'static, ()>,
    directory: tempfile::TempDir,
}

#[cfg(test)]
impl TestCache {
    pub(crate) fn enable() -> Self {
        let serial = lock(&TEST_SERIAL);
        let directory = tempfile::tempdir().expect("temporary discovery cache directory");
        TEST_STATE.with(|state| {
            *state.borrow_mut() = TestState {
                enabled: true,
                directory: Some(directory.path().to_path_buf()),
                ttl: None,
            };
        });
        lock(&MEMO).clear();
        Self {
            _serial: serial,
            directory,
        }
    }

    /// Drop the in-process memo, leaving the on-disk layer intact.
    pub(crate) fn clear_memo(&self) {
        lock(&MEMO).clear();
    }

    fn set_ttl(&self, ttl: Option<u64>) {
        TEST_STATE.with(|state| state.borrow_mut().ttl = ttl);
    }

    fn path_for(&self, organization_url: &str) -> PathBuf {
        self.directory
            .path()
            .join(cache_file_name(organization_url))
    }
}

#[cfg(test)]
impl Drop for TestCache {
    fn drop(&mut self) {
        TEST_STATE.with(|state| *state.borrow_mut() = TestState::default());
        lock(&MEMO).clear();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const ORG: &str = "https://dev.azure.com/acme";

    /// Mirrors live output: `minVersion`/`maxVersion` arrive as JSON numbers,
    /// which is what the old `Option<String>` fields choked on.
    fn discovery_json() -> String {
        r#"[
          {
            "area": "git",
            "id": "e0b8c1d2-0000-4000-8000-000000000000",
            "maxVersion": 7.2,
            "minVersion": 2.1,
            "releasedVersion": "7.1",
            "resourceName": "pullRequestCommits",
            "resourceVersion": 1,
            "routeTemplate": "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/commits"
          }
        ]"#
        .to_string()
    }

    fn counting_fetch(calls: &Cell<usize>) -> impl FnMut() -> Result<String> + '_ {
        move || {
            calls.set(calls.get() + 1);
            Ok(discovery_json())
        }
    }

    #[test]
    fn fetches_once_then_serves_from_memo_and_disk() {
        let cache = TestCache::enable();
        let calls = Cell::new(0);
        let mut fetch = counting_fetch(&calls);

        let first = resource_table(ORG, false, &mut fetch).unwrap();
        assert!(!first.from_cache);
        assert_eq!(first.table.len(), 1);
        assert_eq!(first.table[0].resource_name, "pullRequestCommits");
        assert_eq!(calls.get(), 1);

        assert!(resource_table(ORG, false, &mut fetch).unwrap().from_cache);
        assert_eq!(calls.get(), 1, "expected a process-wide memo hit");

        cache.clear_memo();
        assert!(resource_table(ORG, false, &mut fetch).unwrap().from_cache);
        assert_eq!(calls.get(), 1, "expected the on-disk entry to be reused");
    }

    #[test]
    fn force_refresh_and_expired_ttl_bypass_both_layers() {
        let cache = TestCache::enable();
        let calls = Cell::new(0);
        let mut fetch = counting_fetch(&calls);

        resource_table(ORG, false, &mut fetch).unwrap();
        assert_eq!(calls.get(), 1);

        resource_table(ORG, true, &mut fetch).unwrap();
        assert_eq!(calls.get(), 2, "force_refresh must bypass both layers");

        cache.set_ttl(Some(0));
        cache.clear_memo();
        resource_table(ORG, false, &mut fetch).unwrap();
        assert_eq!(calls.get(), 3, "an expired entry must be refetched");
    }

    #[test]
    fn keeps_organizations_isolated_and_ignores_a_corrupt_file() {
        let cache = TestCache::enable();
        let other = "https://dev.azure.com/other";
        assert_ne!(cache_file_name(ORG), cache_file_name(other));

        let calls = Cell::new(0);
        let mut fetch = counting_fetch(&calls);
        resource_table(ORG, false, &mut fetch).unwrap();
        resource_table(other, false, &mut fetch).unwrap();
        assert_eq!(calls.get(), 2, "each organization needs its own entry");

        std::fs::write(cache.path_for(ORG), "not json").unwrap();
        cache.clear_memo();
        resource_table(ORG, false, &mut fetch).unwrap();
        assert_eq!(calls.get(), 3, "a corrupt file must fall back to a fetch");
    }

    #[test]
    fn disk_entry_for_another_organization_is_rejected() {
        let cache = TestCache::enable();
        let calls = Cell::new(0);
        let mut fetch = counting_fetch(&calls);
        resource_table(ORG, false, &mut fetch).unwrap();

        // Same filename, mismatched payload: the recorded URL must win.
        let path = cache.path_for(ORG);
        let mut cached: CacheFile = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .expect("written cache is readable");
        cached.organization_url = "https://dev.azure.com/somewhere-else".to_string();
        std::fs::write(&path, serde_json::to_string(&cached).unwrap()).unwrap();

        cache.clear_memo();
        resource_table(ORG, false, &mut fetch).unwrap();
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn caching_is_off_by_default_under_test() {
        let _serial = lock(&TEST_SERIAL);
        let calls = Cell::new(0);
        let mut fetch = counting_fetch(&calls);
        resource_table(ORG, false, &mut fetch).unwrap();
        resource_table(ORG, false, &mut fetch).unwrap();
        assert_eq!(
            calls.get(),
            2,
            "scripted az queues rely on caching being inert in tests"
        );
    }

    /// The PR open path fires its independent forge calls concurrently, so a cold
    /// cache must still produce exactly one discovery rather than one per caller.
    #[test]
    fn concurrent_callers_share_a_single_fetch() {
        let cache = TestCache::enable();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let directory = cache.directory.path().to_path_buf();

        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    // Worker threads need the same thread-local override.
                    TEST_STATE.with(|state| {
                        *state.borrow_mut() = TestState {
                            enabled: true,
                            directory: Some(directory.clone()),
                            ttl: None,
                        };
                    });
                    resource_table(ORG, false, || {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        Ok(discovery_json())
                    })
                    .unwrap();
                });
            }
        });

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "concurrent callers must not each trigger discovery"
        );
    }

    #[test]
    fn cache_file_name_is_filesystem_safe_and_bounded() {
        let name = cache_file_name("https://dev.azure.com/acme corp/../etc");
        assert!(
            name.chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '-'
                    || character == '.')
        );
        assert!(!name.contains(".."));
        assert!(name.ends_with(".json"));
        assert!(cache_file_name(&"x".repeat(500)).len() < 120);
    }
}
