//! The pricing cache: immutable blobs, one mutable pointer, and a clock.
//!
//! `docs/decisions/2026-08-21-a-run-with-no-ceiling.md` §6.1 is the contract
//! these tests hold. §6 as originally written said two things that do not
//! compose — the artefacts are immutable and named by their content hash, and
//! a table older than a stated age is refused — and a content hash is not a
//! clock. §6.1 resolves it: `pricing/<sha256>.json` holds the bytes and never
//! changes, `pricing/current.json` says which of them is current and when it
//! arrived, and only the pointer is ever replaced.
//!
//! Every refusal below fails **closed**. A pricing cache exists to bound a
//! guarantee the operator explicitly asked for, so "I could not establish
//! which table this is" must never resolve to "use one anyway".

use std::path::{Path, PathBuf};
use std::time::Duration;

use ego_core::event::time::Timestamp;
use ego_tools::pricing::{CacheError, MAX_AGE, POINTER, PricingCache};
use ego_tools::store::{OsIo, StoreIo};

/// The lease inputs a test uses: this process, and nothing else alive.
///
/// `is_alive` answers `false` for every other pid so a lock file left by a
/// previous test run is takeable rather than a permanent failure.
fn publish(
    cache: &PricingCache<'_>,
    bytes: &[u8],
    version: &str,
    now: u64,
) -> Result<ego_core::cost::PricingRevision, CacheError> {
    cache.publish(
        bytes,
        version,
        Timestamp::from_millis(now),
        std::process::id(),
        &|pid| pid == std::process::id(),
    )
}

fn load(cache: &PricingCache<'_>, now: u64) -> Result<ego_tools::pricing::Cached, CacheError> {
    cache.load(Timestamp::from_millis(now), MAX_AGE)
}

struct Home {
    dir: tempfile::TempDir,
    io: OsIo,
}

impl Home {
    fn new() -> Home {
        Home {
            dir: tempfile::tempdir().expect("a temp dir"),
            io: OsIo,
        }
    }
    fn cache(&self) -> PricingCache<'_> {
        PricingCache::new(&self.io, self.dir.path())
    }
    fn pricing(&self) -> PathBuf {
        self.dir.path().join("pricing")
    }
}

const TABLE: &[u8] = br#"{"gpt-5":{"input_cost_per_token":1e-06}}"#;
const DAY: u64 = 24 * 60 * 60 * 1000;

/// The stored tables, which is not "every file in the directory".
///
/// The pointer lives there, and so does the lock file publication serialises
/// on — a first draft of this counted both as artefacts and failed on the
/// lock, which was the test being wrong rather than a second blob existing.
/// Counting what a blob *is* named like keeps it honest.
fn blobs(home: &Home) -> usize {
    std::fs::read_dir(home.pricing())
        .expect("the directory exists")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name != POINTER && name.ends_with(".json")
        })
        .count()
}

/// The round trip, and the shape on disk.
///
/// Two files and not one: the blob is named by its own hash and the pointer
/// names the blob. That the blob's *filename* is the hash is what makes a
/// later fetch unable to overwrite the bytes an earlier reservation used.
#[test]
fn what_is_published_comes_back_and_the_blob_is_named_by_its_hash() {
    let home = Home::new();
    let cache = home.cache();

    let revision = publish(&cache, TABLE, "etag-1", DAY).expect("publishes");
    let cached = load(&cache, DAY).expect("loads what was just written");

    assert_eq!(cached.bytes, TABLE);
    assert_eq!(cached.revision, revision);

    // The revision names all three facts §6 asks of it: the source, the
    // upstream version, and the content hash.
    let spelled = revision.0;
    assert!(spelled.starts_with("litellm@etag-1#"), "{spelled}");
    let hash = spelled.rsplit('#').next().expect("a hash after the #");
    assert!(
        home.pricing().join(format!("{hash}.json")).exists(),
        "the blob is named by its hash: {spelled}"
    );
    assert!(home.pricing().join(POINTER).exists());
}

/// A second fetch of *different* bytes leaves the first blob where it was.
///
/// This is the property §6 is actually about, and the only one a content hash
/// buys. A `PricingRevision` recorded on a past reservation has to keep naming
/// readable bytes, or the attribution it exists for means nothing.
#[test]
fn a_later_fetch_cannot_overwrite_the_bytes_an_earlier_one_stored() {
    let home = Home::new();
    let cache = home.cache();

    let first = publish(&cache, TABLE, "etag-1", DAY).expect("publishes");
    let other: &[u8] = br#"{"gpt-5":{"input_cost_per_token":2e-06}}"#;
    let second = publish(&cache, other, "etag-2", 2 * DAY).expect("publishes again");

    assert_ne!(first, second, "different bytes, different revision");
    for revision in [&first, &second] {
        let hash = revision.0.rsplit('#').next().expect("a hash");
        assert!(
            home.pricing().join(format!("{hash}.json")).exists(),
            "{revision:?} must still be readable"
        );
    }
    // And the pointer selects the newer one.
    assert_eq!(load(&cache, 2 * DAY).expect("loads").bytes, other);
}

/// Re-publishing identical bytes refreshes the clock without minting a second
/// blob.
///
/// The freshness gate is about when ego last *checked*, not about when the
/// upstream document last changed. A table that is still current after a week
/// is still current, and the fetch that confirmed it is what makes it usable
/// for another week.
#[test]
fn republishing_the_same_bytes_refreshes_the_age_and_adds_no_blob() {
    let home = Home::new();
    let cache = home.cache();

    publish(&cache, TABLE, "etag-1", 0).expect("publishes");
    assert!(
        matches!(load(&cache, 30 * DAY), Err(CacheError::Stale { .. })),
        "a month later it is stale"
    );

    publish(&cache, TABLE, "etag-1", 30 * DAY).expect("publishes again");
    assert!(load(&cache, 30 * DAY).is_ok(), "and now it is not");

    assert_eq!(blobs(&home), 1, "identical bytes are one artefact");
}

/// An empty cache is its own answer, and not an error about a file.
///
/// §7's table sends "missing" and "stale" to the same refusal but they reach
/// it from different places — one operator has never run with a ceiling and
/// the other has an old copy — so the type keeps them apart.
#[test]
fn a_cache_that_was_never_written_says_so() {
    let home = Home::new();
    assert!(matches!(load(&home.cache(), DAY), Err(CacheError::Empty)));
}

/// Past the age, it is refused. Never used with a warning.
///
/// §6: "Refused because `--budget` is a request for a guarantee and a stale
/// table cannot supply one, so the failure must be closed; a warning would be
/// the silently-vacuous shape again, one layer down."
#[test]
fn a_table_older_than_the_maximum_age_is_refused_and_names_its_age() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", 0).expect("publishes");

    let fresh = MAX_AGE.as_millis() as u64 - 1;
    assert!(load(&cache, fresh).is_ok(), "inside the window");

    let err = load(&cache, MAX_AGE.as_millis() as u64 + 1).expect_err("outside it");
    let CacheError::Stale { age, max_age } = err else {
        panic!("wrong error: {err}");
    };
    assert!(age > max_age, "{age:?} vs {max_age:?}");
    assert_eq!(max_age, MAX_AGE);
}

/// The boundary is inclusive: exactly at the age it is still usable.
///
/// Pinned because "older than" and "at least as old as" differ by one
/// millisecond a year, and a reader of §6 cannot tell which was meant.
#[test]
fn a_table_exactly_at_the_maximum_age_is_still_usable() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", 0).expect("publishes");
    assert!(load(&cache, MAX_AGE.as_millis() as u64).is_ok());
}

/// A table stamped in the reader's future fails closed.
///
/// `event/time.rs:22-25` says the wall clock may go backwards — NTP, skew
/// across a subagent process — so this is reachable without anyone tampering.
/// It matters because the alternative is an age computed by underflow, which
/// would present an arbitrarily old table as brand new. Refusing is the only
/// answer that cannot be wrong in the dangerous direction.
#[test]
fn a_table_fetched_in_the_readers_future_is_refused_rather_than_aged_backwards() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", 10 * DAY).expect("publishes");

    let err = load(&cache, DAY).expect_err("the pointer is ahead of this reader");
    assert!(matches!(err, CacheError::FromTheFuture { .. }), "{err}");
}

/// The pointer says *which* artefact, never *what is in it*.
///
/// So the hash is verified on every read. Without this the pointer would be
/// authoritative about content, and a blob corrupted in place — a bad sector,
/// a half-written restore, an editor — would be priced from silently.
#[test]
fn a_blob_whose_bytes_no_longer_match_its_name_is_refused() {
    let home = Home::new();
    let cache = home.cache();
    let revision = publish(&cache, TABLE, "etag-1", DAY).expect("publishes");

    let hash = revision.0.rsplit('#').next().expect("a hash").to_string();
    let blob = home.pricing().join(format!("{hash}.json"));
    std::fs::write(&blob, br#"{"gpt-5":{"input_cost_per_token":9e-06}}"#).expect("tamper");

    let err = load(&cache, DAY).expect_err("the bytes are not the ones named");
    assert!(matches!(err, CacheError::Corrupt { .. }), "{err}");
}

/// A pointer naming a blob that is not there is refused, and nothing is
/// guessed from the directory.
///
/// §6.1: "Nothing scans the directory for a fallback." A cache that fell back
/// to whatever blob it could find would price a run from a table the pointer
/// does not name, which is precisely the attribution the hash exists to make
/// impossible.
#[test]
fn a_pointer_whose_blob_is_gone_does_not_fall_back_to_another_one() {
    let home = Home::new();
    let cache = home.cache();
    let first = publish(&cache, TABLE, "etag-1", DAY).expect("publishes");
    publish(&cache, br#"{"other":{}}"#, "etag-2", DAY).expect("publishes a second");

    // Delete the blob the pointer currently names. The first one is still
    // there, so a fallback has something to find.
    let hash = load(&cache, DAY).expect("loads").revision.0;
    let hash = hash.rsplit('#').next().expect("a hash").to_string();
    std::fs::remove_file(home.pricing().join(format!("{hash}.json"))).expect("remove");

    let err = load(&cache, DAY).expect_err("the named blob is gone");
    assert!(matches!(err, CacheError::Corrupt { .. }), "{err}");
    assert_ne!(
        hash,
        first.0.rsplit('#').next().expect("a hash"),
        "the test only means something if another blob was available"
    );
}

/// A pointer this build does not understand is refused, not read anyway.
///
/// The same rule `RuleId::is_known` applies to a rule from a future grammar:
/// a document from a generation this build cannot verify does not mean
/// something merely because its familiar fields happen to parse.
#[test]
fn a_pointer_from_a_version_this_build_does_not_know_is_refused() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", DAY).expect("publishes");

    let path = home.pricing().join(POINTER);
    let text = std::fs::read_to_string(&path).expect("the pointer");
    let bumped = text.replace("\"v\":1", "\"v\":99");
    assert_ne!(bumped, text, "the pointer carries its version: {text}");
    std::fs::write(&path, bumped).expect("write");

    let err = load(&cache, DAY).expect_err("a version from the future");
    let CacheError::UnknownPointerVersion { found } = err else {
        panic!("wrong error: {err}");
    };
    assert_eq!(found, 99);
}

/// A pointer that is not JSON at all is refused as corruption, not as absence.
///
/// The distinction is load-bearing: `Empty` means "no ceiling has ever been
/// used here" and sends an operator to run a fetch, while a torn pointer means
/// the cache is damaged. Reporting the second as the first would send them to
/// re-run something that already ran.
#[test]
fn a_torn_pointer_is_corruption_and_not_an_empty_cache() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", DAY).expect("publishes");
    std::fs::write(home.pricing().join(POINTER), b"{\"v\":1,\"sha").expect("truncate");

    let err = load(&cache, DAY).expect_err("half a pointer");
    assert!(matches!(err, CacheError::Corrupt { .. }), "{err}");
}

/// A failed publication leaves the previous table exactly where it was.
///
/// §Failure semantics: "a bad fetch never half-replaces a good table". The
/// write order is what buys it — blob first, pointer last — so any failure
/// before the final atomic replace leaves the old pointer selecting the old
/// blob.
#[test]
fn a_publication_that_fails_leaves_the_previous_table_readable() {
    let home = Home::new();
    let cache = home.cache();
    let first = publish(&cache, TABLE, "etag-1", DAY).expect("publishes");

    // A directory where the blob wants to be: the write fails, the pointer is
    // never reached.
    let doomed: &[u8] = br#"{"doomed":{}}"#;
    let hash = ego_core::digest::sha256_hex(doomed);
    std::fs::create_dir(home.pricing().join(format!("{hash}.json"))).expect("squat");

    assert!(
        publish(&cache, doomed, "etag-2", DAY).is_err(),
        "the blob cannot be written"
    );

    let cached = load(&cache, DAY).expect("the old table is untouched");
    assert_eq!(cached.bytes, TABLE);
    assert_eq!(cached.revision, first);
}

/// Two processes cannot publish at once, and the loser is told who holds it.
///
/// `StoreIo`'s scratch path is a pure function of its target
/// (`store/io.rs:35-38`), so two ego processes refreshing this
/// **machine-scoped** cache both write `current.json.tmp` and one can rename a
/// file the other is still writing. The lease is what makes that unreachable.
/// Readers take none: immutable blobs plus an atomic pointer swap need no
/// exclusion.
#[test]
fn a_second_publisher_is_refused_while_the_first_holds_the_lease() {
    let home = Home::new();
    let cache = home.cache();

    let _held = cache
        .lease(Timestamp::from_millis(DAY), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .expect("the first publisher takes it");

    // A different, live pid: not stale, so not takeable.
    let err = cache
        .publish(
            TABLE,
            "etag-1",
            Timestamp::from_millis(DAY),
            std::process::id() + 1,
            &|_| true,
        )
        .expect_err("the lease is held");
    assert!(matches!(err, CacheError::Held { .. }), "{err}");
}

/// And a reader is not blocked by a publisher.
#[test]
fn a_reader_needs_no_lease() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, TABLE, "etag-1", DAY).expect("publishes");

    let _held = cache
        .lease(Timestamp::from_millis(DAY), std::process::id(), &|pid| {
            pid == std::process::id()
        })
        .expect("someone is publishing");

    assert!(load(&cache, DAY).is_ok(), "reads are not excluded");
}

/// Nothing is ever collected, however many fetches have happened.
///
/// A quote carries its `PricingRevision`, and the artefact it names has to
/// stay readable for that attribution to mean anything. Deleting one needs a
/// reachability rule with its own proof, which is not this story's.
#[test]
fn no_fetch_ever_removes_an_earlier_artefact() {
    let home = Home::new();
    let cache = home.cache();
    let mut revisions = Vec::new();
    for n in 1..=5u64 {
        let bytes = format!(r#"{{"n":{n}}}"#).into_bytes();
        revisions.push(publish(&cache, &bytes, &format!("etag-{n}"), n * DAY).expect("publishes"));
    }
    for revision in &revisions {
        let hash = revision.0.rsplit('#').next().expect("a hash");
        assert!(
            home.pricing().join(format!("{hash}.json")).exists(),
            "{revision:?} was collected"
        );
    }
}

/// The cache is under `$EGO_HOME`, machine-scoped, and creates its own home.
///
/// §6: "Machine-scoped, not session-scoped: it is the same table for every
/// session on the host." A first run with a ceiling must not have to be
/// preceded by an `mkdir`.
#[test]
fn the_directory_is_created_on_first_publication() {
    let home = Home::new();
    assert!(!home.pricing().exists(), "nothing yet");
    publish(&home.cache(), TABLE, "etag-1", DAY).expect("publishes");
    assert!(home.pricing().is_dir());
}

/// The blob is written through `StoreIo`, so a build with the durability
/// barrier is the one that ships.
///
/// Asserted through the seam rather than by reading `OsIo`: what matters is
/// that the cache does not reach around `StoreIo` to `std::fs`, because the
/// tmp-then-rename and fsync invariants live behind it and a call site that
/// composed them itself would be the one that forgot half.
#[test]
fn every_byte_goes_through_the_store_seam() {
    let home = Home::new();
    let counted = Counting {
        inner: OsIo,
        writes: std::sync::atomic::AtomicUsize::new(0),
    };
    let cache = PricingCache::new(&counted, home.dir.path());
    publish(&cache, TABLE, "etag-1", DAY).expect("publishes");
    assert!(
        counted.writes.load(std::sync::atomic::Ordering::Relaxed) >= 2,
        "a blob and a pointer"
    );
}

/// A `StoreIo` that counts what was written through it.
struct Counting {
    inner: OsIo,
    writes: std::sync::atomic::AtomicUsize,
}

impl StoreIo for Counting {
    fn append_line(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
        self.inner.append_line(path, line)
    }
    fn fsync(&self, path: &Path) -> std::io::Result<()> {
        self.inner.fsync(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read(path)
    }
    fn exists(&self, path: &Path) -> std::io::Result<bool> {
        self.inner.exists(path)
    }
    fn replace_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.replace_atomic(path, bytes)
    }
    fn truncate(&self, path: &Path, len: u64) -> std::io::Result<()> {
        self.inner.truncate(path, len)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.inner.remove_file(path)
    }
    fn list_dirs(&self, path: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.list_dirs(path)
    }
}

/// The default age is seven days, and it is `ego-tools`' published constant
/// rather than a number repeated at each call site.
#[test]
fn the_default_maximum_age_is_the_week_the_adr_states() {
    assert_eq!(MAX_AGE, Duration::from_secs(7 * 24 * 60 * 60));
}

/// A table too old to *use* is still readable, and reading it is what makes a
/// refresh cost nothing.
///
/// `load` answers one question — "is there a table I may price against" — and
/// a stale one is `Err(Stale)` with the bytes discarded. That is right for a
/// caller about to run, and wrong for the caller that has to go and fix it:
/// refreshing means offering the server back the validator of what is already
/// here, and an unchanged table then costs one round trip and no body. A
/// caller that could not read a stale entry would have to re-download 1.8 MB
/// to discover it already had those exact bytes.
///
/// So `current` is `load` without the age judgement, and `load` is written in
/// terms of it rather than beside it.
#[test]
fn a_stale_table_is_still_readable_so_a_refresh_can_be_conditional() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, b"{}", "\"v1\"", 1_000).expect("published");

    let long_after = 1_000 + MAX_AGE.as_millis() as u64 + 1;
    assert!(
        matches!(load(&cache, long_after), Err(CacheError::Stale { .. })),
        "too old to price against"
    );

    let current = cache
        .current(Timestamp::from_millis(long_after))
        .expect("and still perfectly readable");
    assert_eq!(current.bytes, b"{}");
    assert_eq!(current.fetched_at, Timestamp::from_millis(1_000));
}

/// The validator travels with the bytes, because that is what it is for.
///
/// The revision embeds it, and the revision is deliberately opaque
/// (`cost.rs`), so recovering it by taking a substring would be a caller
/// parsing a type that exists to not be parsed.
#[test]
fn the_cached_table_carries_the_validator_it_was_served_under() {
    let home = Home::new();
    let cache = home.cache();
    publish(&cache, b"{}", "\"8cc8759e\"", 1_000).expect("published");

    let current = cache
        .current(Timestamp::from_millis(2_000))
        .expect("readable");
    assert_eq!(current.upstream_version, "\"8cc8759e\"");
    assert_eq!(
        load(&cache, 2_000).expect("fresh").upstream_version,
        "\"8cc8759e\""
    );
}

/// Everything `load` refuses for a reason other than age, `current` refuses
/// too — it drops one judgement, not the rest of them.
#[test]
fn reading_without_the_age_check_still_fails_closed_on_everything_else() {
    let home = Home::new();
    let cache = home.cache();
    assert!(
        matches!(
            cache.current(Timestamp::from_millis(1_000)),
            Err(CacheError::Empty)
        ),
        "no pointer is no table, at any age"
    );

    publish(&cache, b"{}", "\"v1\"", 5_000).expect("published");
    assert!(
        matches!(
            cache.current(Timestamp::from_millis(1_000)),
            Err(CacheError::FromTheFuture { .. })
        ),
        "a pointer stamped after the reader's clock is not trusted by dropping \
         the age check — that is the check it would have been computed by"
    );
}
