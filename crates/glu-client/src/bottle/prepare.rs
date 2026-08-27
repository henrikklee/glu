use crate::download::VerifiedArtifact;
use crate::hash::{HashingReader, Sha256Mismatch};
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use glu_core::{KegVersion, PackageId, PackageName, Prefix, ResolvedPackage};
use memchr::memmem;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{mpsc, Arc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tar::EntryType;

use crate::bottle::macho::{self, PatchCounts};

use super::{codesign::CodeSignPool, writer::WriterPool};

#[derive(Clone)]
pub struct PrepareInput {
    pub package_id: PackageId,
    pub package: ResolvedPackage,
    pub artifact: VerifiedArtifact,
    pub prefix: Prefix,
    pub writer_pool: Arc<WriterPool>,
    pub code_sign_pool: Arc<CodeSignPool>,
    /// When true, skip Homebrew dynamic-linkage relocation (Mach-O load-command strings).
    /// Only `:any_skip_relocation` bottles qualify (Homebrew's `skip_relocation?`). glu
    /// still runs text placeholder relocation and literal fixed build-prefix relocation so
    /// installed kegs are self-contained and do not accidentally depend on `/opt/homebrew`.
    /// See `docs/explanation/install-pipeline.md`.
    pub skip_relocation: bool,
    /// The literal build-prefix string that bottles of this artifact may contain
    /// (e.g. `/opt/homebrew`), derived from the artifact's fixed cellar (`dirname(cellar)`)
    /// or defaulted to `/opt/homebrew` for marker cellars. Rewritten to `prefix` by the
    /// text or fixed-prefix pass. See `docs/explanation/install-pipeline.md`.
    pub build_prefix: String,
    /// Replacement for the `@@HOMEBREW_JAVA@@` placeholder (Homebrew's java relocation
    /// pair): `<prefix>/opt/<openjdk>/libexec/openjdk.jdk/Contents/Home`, present when the
    /// package declares an openjdk runtime dependency.
    pub java_path: Option<String>,
    /// Replacement for the `@@HOMEBREW_PERL@@` placeholder (Homebrew's perl relocation
    /// pair): `<prefix>/opt/perl/bin/perl` when the package is perl or declares it
    /// directly, else `/usr/bin/perl<built_on.preferred_perl>` if that binary exists,
    /// else `/usr/bin/perl5.34` (current-OS preferred).
    pub perl_path: String,
}

#[derive(Debug, Clone)]
pub struct PreparedKeg {
    pub package_id: PackageId,
    pub name: PackageName,
    pub keg_version: KegVersion,
    pub staging_keg_path: PathBuf,
    pub final_keg_path: PathBuf,
    pub files_to_codesign: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub phase_timings: PreparePhaseTimings,
}

/// Cumulative time spent in each conceptual prepare step (extract / writer_wait /
/// text_relocate / fixed_prefix_relocate / macho_patch / codesign) so traces are directly
/// comparable across tools. glu's extraction is a single streaming pass over tar entries rather
/// than separate whole-keg passes. This deliberate, measured speedup is described in
/// `docs/explanation/install-pipeline.md`. These steps happen interleaved per file instead of as
/// discrete sequential phases; each is still timed individually per file and summed here,
/// giving the same "how much total time did each phase cost" answer without giving up the
/// streaming design.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreparePhaseTimings {
    pub extract: Duration,
    pub writer_wait: Duration,
    pub text_relocate: Duration,
    pub fixed_prefix_relocate: Duration,
    pub macho_patch: Duration,
    pub codesign: Duration,
}

#[derive(Clone)]
struct ReplacementRule {
    old: String,
    new: String,
}

#[derive(Clone)]
struct PrefixProfile {
    prefix: String,
    /// Literal build-prefix string found in bottles (e.g. `/opt/homebrew`), derived from the
    /// artifact's fixed cellar by the orchestrator (`dirname(cellar)`). The fixed-prefix pass
    /// rewrites it to `prefix`; see `docs/explanation/install-pipeline.md`.
    build_prefix: String,
    replacements: Vec<ReplacementRule>,
}
struct StreamPatchResult {
    to_sign: BTreeSet<PathBuf>,
    warnings: Vec<String>,
    timings: PreparePhaseTimings,
}

pub async fn prepare_bottle(input: PrepareInput) -> Result<PreparedKeg> {
    tokio::task::spawn_blocking(move || prepare_bottle_blocking(input))
        .await
        .context("prepare worker panicked")?
}

fn prepare_bottle_blocking(input: PrepareInput) -> Result<PreparedKeg> {
    let staging_root = staging_root(
        &input.prefix,
        &input.package.name,
        &input.package.keg_version,
    );
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)
            .with_context(|| format!("removing stale staging dir {}", staging_root.display()))?;
    }
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("creating staging dir {}", staging_root.display()))?;

    let profile = prefix_profile(
        &input.prefix,
        &input.build_prefix,
        input.java_path.as_deref(),
        &input.perl_path,
    );
    let mut warnings = Vec::new();
    if input.build_prefix != "/opt/homebrew" {
        // The registry holds 26,812 bottle variants with only `/opt/homebrew/Cellar` fixed
        // cellars (0 at any other path); a different build prefix is an anomaly worth
        // surfacing, but we still derive the fixed-prefix pass from the value itself.
        warnings.push(format!(
            "artifact {} has non-standard build prefix {:?}; fixed-prefix relocation derived from the artifact cellar",
            input.package.name.0, input.build_prefix
        ));
    }
    let stream = extract_patch_tar_gz_parallel(
        &input.artifact.path,
        &staging_root,
        profile,
        &input.writer_pool,
        input.skip_relocation,
        &input.artifact.sha256,
    )
    .with_context(|| format!("preparing {}", input.package_id.0))?;
    warnings.extend(stream.warnings);
    let mut phase_timings = stream.timings;

    let codesign_start = Instant::now();
    input.code_sign_pool.sign_all(&stream.to_sign)?;
    phase_timings.codesign = codesign_start.elapsed();

    let staging_keg_path = find_staged_keg(
        &staging_root,
        &input.package.name,
        &input.package.keg_version,
    )?;
    let final_keg_path = input
        .prefix
        .0
        .join("Cellar")
        .join(&input.package.name.0)
        .join(&input.package.keg_version.0);

    Ok(PreparedKeg {
        package_id: input.package_id,
        name: input.package.name,
        keg_version: input.package.keg_version,
        staging_keg_path,
        final_keg_path,
        files_to_codesign: stream.to_sign.into_iter().collect(),
        warnings,
        phase_timings,
    })
}

fn staging_root(prefix: &Prefix, name: &PackageName, keg_version: &KegVersion) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    prefix.0.join("var/glu/staging").join(format!(
        "{}-{}-{}",
        name.0.replace('/', "__"),
        keg_version.0,
        millis
    ))
}

/// Homebrew's perl relocation pair (extend/os/mac/keg_relocate.rb
/// `prepare_relocation_to_locations`): `<prefix>/opt/perl/bin/perl` when the package is
/// perl or declares perl as a direct runtime dependency; else the bottle's
/// `built_on.preferred_perl` (e.g. `5.34`) if `/usr/bin/perl<that>` exists on the
/// current system; else the current-OS preferred perl (`MacOS.preferred_perl_version`:
/// 5.34 on sonoma+ — every supported glu tag).
pub fn perl_relocation_path(
    package_name: &str,
    has_direct_perl_dep: bool,
    built_on_preferred_perl: Option<&str>,
    prefix: &str,
) -> String {
    if package_name == "perl" || has_direct_perl_dep {
        return format!("{prefix}/opt/perl/bin/perl");
    }
    const CURRENT_OS_PERL: &str = "/usr/bin/perl5.34";
    if let Some(version) = built_on_preferred_perl.filter(|v| {
        let mut parts = v.split('.');
        matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(a), Some(b), None)
                if !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.chars().all(|c| c.is_ascii_digit())
        )
    }) {
        let candidate = format!("/usr/bin/perl{version}");
        if Path::new(&candidate).exists() {
            return candidate;
        }
    }
    CURRENT_OS_PERL.to_string()
}

/// The replacement-pair set, mirroring Homebrew's `Relocation` pairs from
/// `prepare_relocation_to_locations` / `prepare_relocation_to_placeholders`
/// (keg_relocate.rb): the prefix/cellar/repository/library placeholders, the
/// perl pair (see `perl_relocation_path`), the java pair when an openjdk dep is
/// declared (extend/os/mac/keg_relocate.rb), and — as a glu extension for
/// fixed-cellar bottles — the literal build-prefix rule consumed by the text
/// and fixed-prefix passes.
fn prefix_profile(
    prefix: &Prefix,
    build_prefix: &str,
    java_path: Option<&str>,
    perl_path: &str,
) -> PrefixProfile {
    let prefix = prefix.0.to_string_lossy().to_string();
    let mut replacements = vec![
        ReplacementRule {
            old: "@@HOMEBREW_PREFIX@@".to_string(),
            new: prefix.clone(),
        },
        ReplacementRule {
            old: "@@HOMEBREW_CELLAR@@".to_string(),
            new: format!("{prefix}/Cellar"),
        },
        ReplacementRule {
            old: "@@HOMEBREW_REPOSITORY@@".to_string(),
            new: prefix.clone(),
        },
        ReplacementRule {
            old: "@@HOMEBREW_LIBRARY@@".to_string(),
            new: format!("{prefix}/Library"),
        },
        ReplacementRule {
            old: "@@HOMEBREW_PERL@@".to_string(),
            new: perl_path.to_string(),
        },
    ];
    if let Some(java) = java_path {
        // Homebrew's java relocation pair (extend/os/mac/keg_relocate.rb
        // prepare_relocation_to_locations): @@HOMEBREW_JAVA@@ ->
        // <prefix>/opt/<openjdk>/libexec/openjdk.jdk/Contents/Home.
        replacements.push(ReplacementRule {
            old: "@@HOMEBREW_JAVA@@".to_string(),
            new: java.to_string(),
        });
    }
    if build_prefix != prefix {
        replacements.push(ReplacementRule {
            old: build_prefix.to_string(),
            new: prefix.clone(),
        });
    }
    PrefixProfile {
        prefix,
        build_prefix: build_prefix.to_string(),
        replacements,
    }
}

fn find_staged_keg(root: &Path, name: &PackageName, keg_version: &KegVersion) -> Result<PathBuf> {
    let direct = root.join(&name.0).join(&keg_version.0);
    if direct.is_dir() {
        return Ok(direct);
    }

    let cellar = root
        .join("opt/homebrew/Cellar")
        .join(&name.0)
        .join(&keg_version.0);
    if cellar.is_dir() {
        return Ok(cellar);
    }

    let mut matches = Vec::new();
    find_dir_named(root, &keg_version.0, &mut matches)?;
    matches.retain(|path| {
        path.parent()
            .and_then(|parent| parent.file_name())
            .and_then(|file_name| file_name.to_str())
            == Some(name.0.as_str())
    });

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!(
            "could not find staged keg for {} {} under {}",
            name.0,
            keg_version.0,
            root.display()
        ),
        _ => bail!(
            "found multiple staged kegs for {} {} under {}",
            name.0,
            keg_version.0,
            root.display()
        ),
    }
}

fn find_dir_named(root: &Path, name: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if path.file_name().and_then(|value| value.to_str()) == Some(name) {
            out.push(path.clone());
        }
        find_dir_named(&path, name, out)?;
    }
    Ok(())
}

/// Streams tar entries once each (read once, patch once — see `WriterPool`'s doc comment for
/// why the write itself goes through a pool rather than inline), matching the benchmark's
/// stream-patch architecture: `gzip/tar reader -> read each regular entry once -> patch bytes
/// before writing -> writer pool writes final bytes -> collect mutated Mach-O files for later
/// codesign`. Writes to a given file's *hardlink* or an `install_name_tool` fixup both
/// need that file to actually be on disk, which the writer pool no longer guarantees
/// synchronously — so both are deferred until this package's own submitted writes have
/// drained, the same ordering guarantee the inline version had for free, restated explicitly
/// here.
fn extract_patch_tar_gz_parallel(
    path: &Path,
    dst: &Path,
    profile: PrefixProfile,
    writer_pool: &WriterPool,
    skip_relocation: bool,
    expected_sha256: &str,
) -> Result<StreamPatchResult> {
    let stream_start = Instant::now();
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    // Fuse artifact verification into the extract read:
    // the whole compressed file passes through a SHA-256 hasher as it is
    // consumed — one pass, no separate re-hash, same primitive as the download
    // ingestion check (crate::hash).
    let gz = GzDecoder::new(HashingReader::new(file));
    let mut archive = tar::Archive::new(gz);
    let mut to_sign = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut violations = Vec::new();
    let mut timings = PreparePhaseTimings::default();
    // One reply channel for this whole package's writes, not one per file — cloning a Sender is
    // cheap, allocating a fresh channel per file is not, at the file counts real bottles hit.
    let (write_reply_tx, write_reply_rx) = mpsc::channel::<Result<()>>();
    let mut pending_writes = 0usize;
    let mut hardlinks = Vec::new();
    // Bottles pack many files per directory (aws-sdk-cpp: 85k regular files under 1.4k unique
    // directories), but the previous implementation called `create_dir_all`
    // unconditionally per file, paying a mkdir+stat syscall pair for a
    // directory that's already there in the overwhelming majority of calls. Track what's already
    // been created this package and skip the redundant ones.
    let mut created_dirs = BTreeSet::new();
    // Track symlinks created by earlier entries. A later entry
    // writing under a symlinked ancestor would follow the link out of the
    // staging root (classic tar-symlink traversal), so every entry's parent
    // chain is checked against this set before any write.
    let mut created_symlinks = BTreeSet::new();

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let rel_path = entry.path().context("entry path")?.into_owned();
        reject_unsafe_path(&rel_path)?;
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        let out = dst.join(&rel_path);
        // Refuse to write through a symlink created by this archive.
        if let Some(link) = symlink_ancestor(&out, &created_symlinks) {
            bail!("archive writes through symlink {}", link.display());
        }
        let kind = entry.header().entry_type();

        if kind == EntryType::Directory {
            ensure_dir_created(&mut created_dirs, &out)?;
        } else if kind == EntryType::Regular {
            if let Some(parent) = out.parent() {
                ensure_dir_created(&mut created_dirs, parent)?;
            }
            let entry_size = entry.size();
            let writer_wait_start = Instant::now();
            let memory = writer_pool.reserve_bytes(entry_size);
            timings.writer_wait += writer_wait_start.elapsed();
            let mut data = Vec::with_capacity((entry_size as usize).min(MAX_ENTRY_PREALLOC));
            entry
                .read_to_end(&mut data)
                .with_context(|| format!("read entry {}", rel_path.display()))?;
            let mode = entry.header().mode().ok();

            let mut macho = false;
            let mut mutated_macho_load_commands = false;
            if !skip_relocation {
                let macho_start = Instant::now();
                let mut counts = PatchCounts::default();
                let (is_macho, mutated) = macho::patch_macho(
                    &rel,
                    &mut data,
                    &profile.prefix,
                    &mut counts,
                    &mut warnings,
                )?;
                macho = is_macho;
                mutated_macho_load_commands = mutated;
                timings.macho_patch += macho_start.elapsed();
            }

            // Homebrew's :any_skip_relocation skips dynamic-linkage relocation. glu still
            // relocates literal build-prefix bytes so the prepared keg is self-contained
            // and does not accidentally depend on the user's machine having /opt/homebrew.
            let fixed_prefix_start = Instant::now();
            let mutated_fixed_prefix =
                patch_fixed_prefix_bytes(&rel, &mut data, &profile, &mut warnings);
            timings.fixed_prefix_relocate += fixed_prefix_start.elapsed();

            if skip_relocation && mutated_fixed_prefix {
                // Avoid a Mach-O sniff on every skip-relocation file; only changed files
                // need classification for ad-hoc signing.
                macho = macho::is_macho(&data);
            }

            if macho && (mutated_macho_load_commands || mutated_fixed_prefix) {
                to_sign.insert(out.clone());
            }

            // Text relocation always runs (skip bottles included — see the comment above
            // the skip gate). Homebrew's `text_files` includes `text_executable?` files
            // even when `file(1)` reports `data`; this matters for script/archive hybrids
            // such as php's `phar.phar`, whose shebang contains a placeholder before the
            // binary phar payload begins. Mach-O files are still excluded by `macho`.
            if !macho && (is_probably_text(&data) || is_text_executable(&data)) {
                let text_start = Instant::now();
                patch_text_bytes(&mut data, &profile);
                timings.text_relocate += text_start.elapsed();
            }

            // Per-install verification: after the patch
            // passes, no placeholder or build-prefix literal may survive in any bottle
            // (skip bottles included — gradle's text placeholders are caught here if the
            // text pass misses them). A miss means a relocation rule we didn't apply —
            // fail-closed instead of shipping broken bytes.
            if let Some(pos) = find_leftover_placeholder(&data) {
                violations.push(format!(
                    "{rel}: unresolved @@HOMEBREW_* placeholder at byte offset {pos:#x}"
                ));
            }
            if profile.prefix != profile.build_prefix {
                if let Some(pos) =
                    find_leftover_build_prefix(&data, profile.build_prefix.as_bytes())
                {
                    violations.push(format!(
                        "{rel}: unresolved build-prefix {:?} bytes at offset {pos:#x}",
                        profile.build_prefix
                    ));
                }
            }

            let writer_wait_start = Instant::now();
            writer_pool.submit_extracted(out, data, mode, write_reply_tx.clone(), memory)?;
            timings.writer_wait += writer_wait_start.elapsed();
            pending_writes += 1;
        } else if kind == EntryType::Symlink {
            if let Some(parent) = out.parent() {
                ensure_dir_created(&mut created_dirs, parent)?;
            }
            let Some(target) = entry.link_name().context("symlink target")? else {
                continue;
            };
            create_symlink(&target, &out)
                .with_context(|| format!("symlink {} -> {}", out.display(), target.display()))?;
            created_symlinks.insert(out.clone());
        } else if kind == EntryType::Link {
            // Deferred below: tar always lists a hardlink's target before the hardlink entry
            // itself, but the target's *write* may still be sitting in the shared pool's queue.
            if let Some(parent) = out.parent() {
                ensure_dir_created(&mut created_dirs, parent)?;
            }
            let Some(target) = entry.link_name().context("hardlink target")? else {
                continue;
            };
            // A hardlink target is resolved to a file
            // later (read/copy), so its lexical path must remain within the
            // staging root. Symlink targets are resolved and re-confined after
            // pending writes drain, preserving safe keg-internal symlink chains
            // while rejecting escapes.
            let src = hardlink_target_path(&target, dst)?;
            hardlinks.push((out, src));
        }
    }

    drop(write_reply_tx);
    let writer_wait_start = Instant::now();
    for _ in 0..pending_writes {
        write_reply_rx
            .recv()
            .context("writer pool disconnected")??;
    }
    timings.writer_wait += writer_wait_start.elapsed();
    for (out, src) in hardlinks {
        let resolved_src = resolve_hardlink_source(dst, &src)?;
        fs::hard_link(&resolved_src, &out)
            .or_else(|_| fs::copy(&resolved_src, &out).map(|_| ()))
            .with_context(|| {
                format!(
                    "hardlink/copy {} -> {}",
                    out.display(),
                    resolved_src.display()
                )
            })?;
    }

    // Fused verification (S4): drain the remaining decompressed stream so the
    // hasher has seen the gzip footer and any trailing bytes — i.e. the entire
    // compressed artifact — then compare against the declared sha256 before
    // anything is committed. Fail-closed on mismatch, cache hit or not.
    let mut gz = archive.into_inner();
    std::io::copy(&mut gz, &mut std::io::sink())
        .with_context(|| format!("reading {} to end", path.display()))?;
    let actual = gz.into_inner().finish();
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(
            Sha256Mismatch::new(path.display().to_string(), expected_sha256, actual).into(),
        );
    }

    if !violations.is_empty() {
        // Fail-closed verification: prepare failure means the
        // package is not installed; the caller cleans up the staging dir.
        bail!(
            "relocation leftovers in {} file(s); first: {}",
            violations.len(),
            violations
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    // Relocation and shared-writer backpressure happen inside the streaming extraction loop.
    // Charge their measured durations only to their own phases, leaving decode, tar parsing,
    // validation, hashing, and inline filesystem metadata work as exclusive extraction time.
    timings.extract = exclusive_extract_duration(stream_start.elapsed(), &timings);

    Ok(StreamPatchResult {
        to_sign,
        warnings,
        timings,
    })
}

fn exclusive_extract_duration(total: Duration, timings: &PreparePhaseTimings) -> Duration {
    total.saturating_sub(
        timings.writer_wait
            + timings.text_relocate
            + timings.fixed_prefix_relocate
            + timings.macho_patch,
    )
}

fn ensure_dir_created(created: &mut BTreeSet<PathBuf>, dir: &Path) -> Result<()> {
    if created.contains(dir) {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    created.insert(dir.to_path_buf());
    Ok(())
}

// Load-command relocation moved to `macho.rs` (see `macho::patch_macho`);
// prepare.rs keeps the streaming tar loop and the text / fixed-prefix passes.

fn patch_fixed_prefix_bytes(
    rel: &str,
    data: &mut Vec<u8>,
    profile: &PrefixProfile,
    warnings: &mut Vec<String>,
) -> bool {
    let old = profile.build_prefix.as_bytes();
    let new = profile.prefix.as_bytes();
    if old == new {
        // Identity rewrite (installing into the same prefix the bottle was built for):
        // nothing to do, and don't mark the file for re-signing.
        return false;
    }
    if !data.contains(&0) {
        // Homebrew gates on binary_file? (contains a NUL byte); NUL-free text
        // files are handled by the text pass without padding.
        return false;
    }
    if is_text_executable(data) {
        // Homebrew's relocate_build_prefix skips "sharballs"/text executables.
        // They may still contain NUL bytes (php's phar.phar), but their text
        // shebang/stub must be relocated by the text pass rather than by
        // NUL-delimited binary padding.
        return false;
    }
    if old.len() == new.len() {
        // Official installs enforce this invariant. Replace directly in the extraction buffer:
        // rebuilding NUL-delimited strings copied hundreds of MiB for GCC even though no byte
        // needed to move. The independent leftover check below the caller remains the fail-closed
        // verification boundary.
        return replace_all_equal_length_in_place(data, old, new);
    }
    if memmem::find(data, old).is_none() {
        return false;
    }
    if new.len() > old.len() {
        warnings.push(format!(
            "cannot grow fixed-prefix binary string in {rel}: {:?} -> {:?}",
            profile.build_prefix, profile.prefix
        ));
        return false;
    }

    // Retain Homebrew's NUL-piece padding semantics for unsupported unequal-length dev prefixes.
    let mut out = Vec::with_capacity(data.len());
    let mut mutated = false;
    // Iterate NUL-delimited pieces (Homebrew: binary.split(/\x00/, -1)).
    let mut piece_start = 0;
    for (i, &b) in data.iter().enumerate() {
        if b == 0 {
            push_rewritten_piece(&mut out, &data[piece_start..i], old, new, &mut mutated);
            out.push(0);
            piece_start = i + 1;
        }
    }
    push_rewritten_piece(&mut out, &data[piece_start..], old, new, &mut mutated);
    debug_assert_eq!(
        out.len(),
        data.len(),
        "fixed-prefix relocation must be size-preserving"
    );
    *data = out;
    mutated
}

fn replace_all_equal_length_in_place(data: &mut [u8], old: &[u8], new: &[u8]) -> bool {
    debug_assert!(!old.is_empty());
    debug_assert_eq!(old.len(), new.len());

    let finder = memmem::Finder::new(old);
    let mut cursor = 0;
    let mut mutated = false;
    while let Some(relative) = finder.find(&data[cursor..]) {
        let found = cursor + relative;
        data[found..found + old.len()].copy_from_slice(new);
        cursor = found + old.len();
        mutated = true;
    }
    mutated
}

/// Rewrite `piece`, then pad it back to its original length with NULs
/// (Homebrew's `ljust(s.size, NULL_BYTE)`), preserving the whole-file size.
fn push_rewritten_piece(
    out: &mut Vec<u8>,
    piece: &[u8],
    old: &[u8],
    new: &[u8],
    mutated: &mut bool,
) {
    let count = memmem::find_iter(piece, old).count();
    if count == 0 {
        out.extend_from_slice(piece);
        return;
    }
    *mutated = true;
    let mut pos = 0;
    for found in memmem::find_iter(piece, old) {
        out.extend_from_slice(&piece[pos..found]);
        out.extend_from_slice(new);
        pos = found + old.len();
    }
    out.extend_from_slice(&piece[pos..]);
    out.resize(out.len() + count * (old.len() - new.len()), 0);
}

fn patch_text_bytes(data: &mut Vec<u8>, profile: &PrefixProfile) -> usize {
    let mut replacements = 0;
    for rule in &profile.replacements {
        let old = rule.old.as_bytes();
        if memmem::find(data, old).is_none() {
            continue;
        }
        *data = replace_all(data.as_slice(), old, rule.new.as_bytes(), &mut replacements);
    }
    replacements
}

fn replace_all(data: &[u8], old: &[u8], new: &[u8], replacements: &mut usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0;
    for found in memmem::find_iter(data, old) {
        out.extend_from_slice(&data[pos..found]);
        out.extend_from_slice(new);
        pos = found + old.len();
        *replacements += 1;
    }
    out.extend_from_slice(&data[pos..]);
    out
}

/// No `@@HOMEBREW_*` placeholder bytes may survive prepare in a non-skip bottle.
fn find_leftover_placeholder(data: &[u8]) -> Option<usize> {
    memmem::find(data, b"@@HOMEBREW_")
}

/// No literal build-prefix bytes may survive when the install prefix differs from the
/// build prefix. If they are the same (theoretical `/opt/homebrew` target), the check yields.
fn find_leftover_build_prefix(data: &[u8], build_prefix: &[u8]) -> Option<usize> {
    memmem::find(data, build_prefix)
}

/// NUL-free heuristic for the text pass: files whose first 8 KiB contain no NUL byte
/// are treated as text for placeholder substitution.
fn is_probably_text(data: &[u8]) -> bool {
    !data[..data.len().min(8192)].contains(&0)
}

/// Homebrew `Pathname#text_executable?` parity: a file whose first 1024 bytes match
/// `/\A#!\s*\S+/` is text-relocatable even if it later contains NUL bytes (script/archive
/// hybrids such as php's phar.phar).
fn is_text_executable(data: &[u8]) -> bool {
    let head = &data[..data.len().min(1024)];
    let Some(rest) = head.strip_prefix(b"#!") else {
        return false;
    };
    rest.iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .any(|b| !b.is_ascii_whitespace())
}

fn reject_unsafe_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("unsafe archive path: {}", path.display());
    }
    Ok(())
}

/// Upper bound for the eager preallocation of a tar entry's buffer (S8). The
/// header's claimed size is attacker/mirror-controlled; a lying huge value
/// must not trigger a giant allocation. `read_to_end` still grows the buffer
/// if a file is legitimately larger, so capping only constrains the reserve.
const MAX_ENTRY_PREALLOC: usize = 1 << 30; // 1 GiB

/// Whether any ancestor of `out` (below the staging root) is a symlink created
/// by an earlier entry in this archive — the write-through traversal guard.
fn symlink_ancestor(out: &Path, symlinks: &BTreeSet<PathBuf>) -> Option<PathBuf> {
    let mut parent = out.parent();
    while let Some(dir) = parent {
        if symlinks.contains(dir) {
            return Some(dir.to_path_buf());
        }
        parent = dir.parent();
    }
    None
}

/// A hardlink target is resolved to a real file during
/// extraction, so it must be a relative path that resolves (lexically, with
/// `..` popped) inside `root` — never absolute, never escaping.
fn hardlink_target_path(target: &Path, root: &Path) -> Result<PathBuf> {
    if target.is_absolute() {
        bail!("unsafe archive hardlink target: {}", target.display());
    }
    let root = normalize_lexical(root)?;
    let resolved = normalize_lexical(&root.join(target))?;
    if !resolved.starts_with(&root) {
        bail!("unsafe archive hardlink target: {}", target.display());
    }
    Ok(resolved)
}

/// Resolve a hardlink source that may itself be a symlink created by the
/// archive. Each symlink hop is lexically normalized and re-confined to the
/// staging root before following it, so safe keg-internal symlink targets keep
/// working while symlink escapes cannot copy host files into the keg.
fn resolve_hardlink_source(root: &Path, src: &Path) -> Result<PathBuf> {
    const MAX_SYMLINK_DEPTH: usize = 40;

    let root = normalize_lexical(root)?;
    let mut current = normalize_lexical(src)?;
    if !current.starts_with(&root) {
        bail!("unsafe archive hardlink target: {}", src.display());
    }

    for _ in 0..MAX_SYMLINK_DEPTH {
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("stat hardlink target {}", current.display()))?;
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let target = fs::read_link(&current)
            .with_context(|| format!("read symlink target {}", current.display()))?;
        let next = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(target)
        };
        current = normalize_lexical(&next)?;
        if !current.starts_with(&root) {
            bail!(
                "unsafe archive hardlink symlink target: {}",
                current.display()
            );
        }
    }

    bail!("archive hardlink symlink chain too deep: {}", src.display())
}

fn normalize_lexical(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => bail!("unsupported path prefix: {}", path.display()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("path escapes root: {}", path.display());
                }
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    if link.exists() || link.is_symlink() {
        fs::remove_file(link)
            .with_context(|| format!("removing existing symlink {}", link.display()))?;
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(prefix: &str) -> PrefixProfile {
        prefix_profile(
            &Prefix(PathBuf::from(prefix)),
            "/opt/homebrew",
            None,
            "/usr/bin/perl5.34",
        )
    }

    #[test]
    fn exclusive_extract_timing_subtracts_non_extract_work() {
        let timings = PreparePhaseTimings {
            extract: Duration::ZERO,
            writer_wait: Duration::from_millis(4),
            text_relocate: Duration::from_millis(2),
            fixed_prefix_relocate: Duration::from_millis(3),
            macho_patch: Duration::from_millis(5),
            codesign: Duration::from_millis(7),
        };

        assert_eq!(
            exclusive_extract_duration(Duration::from_millis(30), &timings),
            Duration::from_millis(16)
        );
        assert_eq!(
            exclusive_extract_duration(Duration::from_millis(5), &timings),
            Duration::ZERO
        );
    }

    #[test]
    fn fixed_prefix_keeps_nul_terminated_strings_intact() {
        // A Mach-O install-name style string: the replacement must not truncate
        // the continuation after the old prefix (equal-length prefix here).
        let mut data = b"/opt/homebrew/opt/dep/lib/libdep.dylib\0".to_vec();
        let mut warnings = Vec::new();
        let mutated =
            patch_fixed_prefix_bytes("test", &mut data, &profile("/opt/glustore"), &mut warnings);
        assert!(mutated);
        assert!(warnings.is_empty());
        assert_eq!(data, b"/opt/glustore/opt/dep/lib/libdep.dylib\0".to_vec());
    }

    #[test]
    fn fixed_prefix_equal_length_rewrites_in_place_with_byte_parity() {
        let mut data = b"/opt/homebrew/a:/opt/homebrew/b\0middle\0tail:/opt/homebrew\0".to_vec();
        let original_allocation = data.as_ptr();
        let original_len = data.len();
        let mut replacements = 0;
        let expected = replace_all(&data, b"/opt/homebrew", b"/opt/glustore", &mut replacements);
        let mut warnings = Vec::new();

        let mutated =
            patch_fixed_prefix_bytes("test", &mut data, &profile("/opt/glustore"), &mut warnings);

        assert!(mutated);
        assert!(warnings.is_empty());
        assert_eq!(replacements, 3);
        assert_eq!(data, expected);
        assert_eq!(data.len(), original_len);
        assert_eq!(
            data.as_ptr(),
            original_allocation,
            "equal-length relocation must reuse the extraction buffer"
        );
    }

    #[test]
    fn fixed_prefix_pads_shorter_prefix_at_piece_end() {
        // New prefix shorter than old: padding goes at the END of the
        // NUL-delimited piece (Homebrew ljust), never mid-string.
        let mut data = b"/opt/homebrew/lib/foo.dylib\0/opt/homebrew\0".to_vec();
        let mut warnings = Vec::new();
        let mutated = patch_fixed_prefix_bytes("test", &mut data, &profile("/x"), &mut warnings);
        assert!(mutated);
        assert!(warnings.is_empty());
        let mut expected = b"/x/lib/foo.dylib".to_vec();
        expected.resize(27, 0); // piece 1 padded back to its original 27 bytes
        expected.push(0); // separator
        expected.extend_from_slice(b"/x");
        expected.resize(42, 0); // piece 2 padded back to 13 bytes, + trailing separator
        assert_eq!(data, expected);
        assert_eq!(data.len(), 42);
    }

    #[test]
    fn fixed_prefix_skips_text_files() {
        // No NUL byte => Homebrew's binary_file? is false; the text pass owns
        // this file and pads nothing.
        let mut data = b"#!/opt/homebrew/bin/python3\n".to_vec();
        let mut warnings = Vec::new();
        let mutated =
            patch_fixed_prefix_bytes("test", &mut data, &profile("/opt/glustore"), &mut warnings);
        assert!(!mutated);
        assert!(warnings.is_empty());
        assert_eq!(data, b"#!/opt/homebrew/bin/python3\n".to_vec());
    }

    #[test]
    fn fixed_prefix_noop_when_prefix_absent() {
        let mut data = b"no prefix here\0\0".to_vec();
        let mut warnings = Vec::new();
        let mutated = patch_fixed_prefix_bytes("test", &mut data, &profile("/x"), &mut warnings);
        assert!(!mutated);
        assert!(warnings.is_empty());
        assert_eq!(data, b"no prefix here\0\0".to_vec());
    }

    #[test]
    fn fixed_prefix_identity_rewrite_is_a_noop() {
        // Installing into the same prefix the bottle was built for: old == new, so the pass
        // must not run and must not mark the file for re-signing.
        let mut data = b"/opt/homebrew/etc/openssl@3/openssl.cnf\0\0".to_vec();
        let mut warnings = Vec::new();
        let mutated =
            patch_fixed_prefix_bytes("test", &mut data, &profile("/opt/homebrew"), &mut warnings);
        assert!(!mutated);
        assert!(warnings.is_empty());
        assert_eq!(
            data,
            b"/opt/homebrew/etc/openssl@3/openssl.cnf\0\0".to_vec()
        );
    }

    #[test]
    fn profile_omits_identity_literal_build_prefix_rule() {
        let profile = prefix_profile(
            &Prefix(PathBuf::from("/opt/homebrew")),
            "/opt/homebrew",
            None,
            "/usr/bin/perl5.34",
        );
        assert!(profile
            .replacements
            .iter()
            .all(|r| !(r.old == "/opt/homebrew" && r.new == "/opt/homebrew")));
    }

    #[test]
    fn profile_includes_java_rule_when_openjdk_dep_present() {
        let profile = prefix_profile(
            &Prefix(PathBuf::from("/opt/glustore")),
            "/opt/homebrew",
            Some("/opt/glustore/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"),
            "/usr/bin/perl5.34",
        );
        let java = profile
            .replacements
            .iter()
            .find(|r| r.old == "@@HOMEBREW_JAVA@@")
            .expect("java rule present");
        assert_eq!(
            java.new,
            "/opt/glustore/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"
        );

        let without_java = prefix_profile(
            &Prefix(PathBuf::from("/opt/glustore")),
            "/opt/homebrew",
            None,
            "/usr/bin/perl5.34",
        );
        assert!(without_java
            .replacements
            .iter()
            .all(|r| r.old != "@@HOMEBREW_JAVA@@"));
    }

    #[test]
    fn text_relocation_replaces_literal_build_prefix_unconditionally() {
        let mut data = b"-R/opt/homebrew/opt/openssl@3/lib\nabc/opt/homebrew2/lib/x\n".to_vec();
        let replacements = patch_text_bytes(&mut data, &profile("/opt/glustore"));
        assert_eq!(replacements, 2);
        assert_eq!(
            data,
            b"-R/opt/glustore/opt/openssl@3/lib\nabc/opt/glustore2/lib/x\n".to_vec()
        );
        assert!(find_leftover_build_prefix(&data, b"/opt/homebrew").is_none());
    }

    #[test]
    fn phar_like_text_executable_with_nuls_gets_text_relocated() {
        let binary_tail = b"\x36\x01\x00\x00pharcommand\x00\x00\x00payload";
        let marker = b"__HALT_COMPILER(); ?>\r\n";
        let mut data = b"#!@@HOMEBREW_CELLAR@@/php/8.5.9/bin/php\n<?php\n".to_vec();
        data.extend_from_slice(marker);
        data.extend_from_slice(binary_tail);

        assert!(!is_probably_text(&data));
        assert!(is_text_executable(&data));

        let mut warnings = Vec::new();
        assert!(!patch_fixed_prefix_bytes(
            "php/8.5.9/bin/phar.phar",
            &mut data.clone(),
            &profile("/opt/glustore"),
            &mut warnings
        ));
        assert!(warnings.is_empty());

        let replacements = patch_text_bytes(&mut data, &profile("/opt/glustore"));
        assert_eq!(replacements, 1);
        assert!(data.starts_with(b"#!/opt/glustore/Cellar/php/8.5.9/bin/php\n"));
        assert!(find_leftover_placeholder(&data).is_none());

        let tail_start = memmem::find(&data, marker).expect("marker survives") + marker.len();
        assert_eq!(&data[tail_start..], binary_tail);
    }

    #[test]
    fn leftover_finders_detect_unresolved_bytes() {
        assert!(find_leftover_placeholder(b"prefix @@HOMEBREW_JAVA@@ done").is_some());
        assert!(find_leftover_placeholder(b"all clean, no placeholders").is_none());
        assert!(
            find_leftover_build_prefix(b"/opt/homebrew/Cellar/x\0", b"/opt/homebrew").is_some()
        );
        assert!(
            find_leftover_build_prefix(b"/opt/glustore/Cellar/x\0", b"/opt/homebrew").is_none()
        );
    }

    #[test]
    fn perl_relocation_prefers_opt_perl_for_perl_and_direct_deps() {
        assert_eq!(
            perl_relocation_path("perl", false, None, "/opt/glustore"),
            "/opt/glustore/opt/perl/bin/perl"
        );
        assert_eq!(
            perl_relocation_path("git", true, Some("5.30"), "/opt/glustore"),
            "/opt/glustore/opt/perl/bin/perl"
        );
    }

    #[test]
    fn perl_relocation_uses_built_on_version_when_it_exists() {
        // 5.34 exists on this macOS (sonoma+); 9.99 does not.
        assert_eq!(
            perl_relocation_path("git", false, Some("5.34"), "/opt/glustore"),
            "/usr/bin/perl5.34"
        );
        assert_eq!(
            perl_relocation_path("git", false, Some("9.99"), "/opt/glustore"),
            "/usr/bin/perl5.34",
            "non-existent built_on version falls back to the current-OS perl"
        );
    }

    #[test]
    fn perl_relocation_falls_back_without_built_on_or_malformed() {
        assert_eq!(
            perl_relocation_path("git", false, None, "/opt/glustore"),
            "/usr/bin/perl5.34"
        );
        for malformed in ["5", "5.3.4", "abc", ".34", "5."] {
            assert_eq!(
                perl_relocation_path("git", false, Some(malformed), "/opt/glustore"),
                "/usr/bin/perl5.34",
                "malformed built_on version {malformed:?} must fall back"
            );
        }
    }

    #[test]
    fn profile_includes_perl_rule_with_computed_value() {
        let profile = prefix_profile(
            &Prefix(PathBuf::from("/opt/glustore")),
            "/opt/homebrew",
            None,
            "/usr/bin/perl5.34",
        );
        let perl = profile
            .replacements
            .iter()
            .find(|r| r.old == "@@HOMEBREW_PERL@@")
            .expect("perl rule present");
        assert_eq!(perl.new, "/usr/bin/perl5.34");
    }
}

#[cfg(test)]
mod extraction_safety_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn hardlink_target_within_root_is_accepted() {
        let root = Path::new("/tmp/glustore/var/glu/staging/x");
        for good in ["bin/foo", "lib/../lib/libfoo.dylib", "a/./b"] {
            let target = Path::new(good);
            assert!(
                hardlink_target_path(target, root).is_ok(),
                "expected {good:?} to be accepted"
            );
        }
    }

    #[test]
    fn hardlink_target_escaping_root_is_rejected() {
        let root = Path::new("/tmp/glustore/var/glu/staging/x");
        for bad in ["/etc/passwd", "../outside", "a/../../etc/passwd"] {
            let target = Path::new(bad);
            assert!(
                hardlink_target_path(target, root).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn hardlink_source_resolves_safe_internal_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("staging");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("real"), "ok").unwrap();
        std::os::unix::fs::symlink("../real", root.join("dir/link")).unwrap();

        let resolved = resolve_hardlink_source(&root, &root.join("dir/link")).unwrap();

        assert_eq!(resolved, root.join("real"));
    }

    #[test]
    #[cfg(unix)]
    fn hardlink_source_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("staging");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink("../../outside", root.join("link")).unwrap();

        let err = resolve_hardlink_source(&root, &root.join("link")).unwrap_err();

        assert!(err.to_string().contains("unsafe archive hardlink"));
    }

    #[test]
    fn symlink_ancestor_detects_write_through() {
        let root = Path::new("/tmp/x");
        let mut symlinks = BTreeSet::new();
        symlinks.insert(root.join("evil"));
        // `evil/pwned` has `evil` as an ancestor -> flagged.
        assert!(symlink_ancestor(&root.join("evil/pwned"), &symlinks).is_some());
        // A sibling path is not under a symlink.
        assert!(symlink_ancestor(&root.join("safe/pwned"), &symlinks).is_none());
        // The symlink entry itself (no deeper parent) is fine.
        assert!(symlink_ancestor(&root.join("evil"), &symlinks).is_none());
    }
}

#[cfg(test)]
mod fused_verify_tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use glu_core::Prefix;

    /// Builds a small valid .tar.gz (one regular file `hello.txt`) containing
    /// `content`.
    fn build_artifact(dir: &Path, tag: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(format!("{tag}.tar.gz"));
        let file = fs::File::create(&path).unwrap();
        let enc = GzEncoder::new(file, Compression::default());
        let mut tar = tar::Builder::new(enc);
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "hello.txt", content).unwrap();
        tar.finish().unwrap();
        let enc = tar.into_inner().unwrap();
        enc.finish().unwrap();
        path
    }

    fn sha_file(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        crate::hash::sha256_hex(&bytes)
    }

    fn profile_with(prefix: &str) -> PrefixProfile {
        prefix_profile(
            &Prefix(PathBuf::from(prefix)),
            "/opt/homebrew",
            None,
            "/usr/bin/perl5.34",
        )
    }

    fn profile() -> PrefixProfile {
        profile_with("/opt/homebrew")
    }

    #[test]
    fn matching_artifact_extracts_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = build_artifact(dir.path(), "ok", b"content");
        let dst = dir.path().join("out");
        let expected = sha_file(&artifact);
        let pool = WriterPool::new();
        let res = extract_patch_tar_gz_parallel(&artifact, &dst, profile(), &pool, true, &expected);
        assert!(
            res.is_ok(),
            "extraction should succeed for a matching artifact"
        );
        assert!(dst.join("hello.txt").is_file());
    }

    #[test]
    fn streaming_phase_durations_do_not_double_count_relocation() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = build_artifact(
            dir.path(),
            "timing",
            b"#!@@HOMEBREW_PREFIX@@/bin/sh\necho ok\n",
        );
        let dst = dir.path().join("out");
        let expected = sha_file(&artifact);
        let pool = WriterPool::new();
        let wall_start = Instant::now();
        let result = extract_patch_tar_gz_parallel(
            &artifact,
            &dst,
            profile_with("/opt/glustore"),
            &pool,
            false,
            &expected,
        )
        .unwrap();
        let wall = wall_start.elapsed();
        let timings = result.timings;
        let phase_sum = timings.extract
            + timings.writer_wait
            + timings.text_relocate
            + timings.fixed_prefix_relocate
            + timings.macho_patch;

        assert!(timings.writer_wait > Duration::ZERO);
        assert!(timings.text_relocate > Duration::ZERO);
        assert!(
            phase_sum <= wall,
            "phase sum {phase_sum:?} exceeded wall {wall:?}"
        );
        assert_eq!(
            fs::read(dst.join("hello.txt")).unwrap(),
            b"#!/opt/glustore/bin/sh\necho ok\n"
        );
    }

    #[test]
    fn skip_relocation_still_applies_fixed_build_prefix_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = build_artifact(dir.path(), "skip-fixed-prefix", b"/opt/homebrew/bin/qwen\0");
        let expected = sha_file(&artifact);
        let dst = dir.path().join("out");
        let pool = WriterPool::new();
        let res = extract_patch_tar_gz_parallel(
            &artifact,
            &dst,
            profile_with("/opt/glustore"),
            &pool,
            true,
            &expected,
        );
        if let Err(err) = res {
            panic!(
                "skip-relocation bottles must still relocate literal build-prefix bytes: {err:#}"
            );
        }
        assert_eq!(
            fs::read(dst.join("hello.txt")).unwrap(),
            b"/opt/glustore/bin/qwen\0"
        );
    }

    #[test]
    fn substituted_artifact_fails_closed_on_hash() {
        // A valid gzip artifact whose bytes don't match the declared sha256
        // (the "wrong bottle substituted for a cache hit" case): extraction
        // decodes fine but the fused hash check must reject it.
        let dir = tempfile::tempdir().unwrap();
        let good = build_artifact(dir.path(), "good", b"content");
        let evil = build_artifact(dir.path(), "evil", b"CONTENT"); // valid but different
        let expected = sha_file(&good); // declared sha of the *good* artifact
        let dst = dir.path().join("out");
        let pool = WriterPool::new();
        let res = extract_patch_tar_gz_parallel(&evil, &dst, profile(), &pool, true, &expected);
        let Err(err) = res else {
            panic!("substituted artifact must be rejected");
        };
        let msg = format!("{err}");
        assert!(msg.contains("sha256 mismatch"), "got: {msg}");
        // The extracted file may still be in the staging dir (verification runs
        // fused at the end of the read); the caller discards staging on the
        // error, so the bytes never reach a committed keg.
    }

    #[test]
    fn tampered_artifact_fails_closed() {
        // A byte flipped in the middle reliably breaks the gzip stream; that
        // must also fail-closed (never commit unverified bytes).
        let dir = tempfile::tempdir().unwrap();
        let artifact = build_artifact(dir.path(), "tampered", b"content");
        let mut bytes = fs::read(&artifact).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        fs::write(&artifact, &bytes).unwrap();

        // Use the declared hash of a pristine copy (the tampered file's own
        // recomputed hash would trivially match it).
        let original = build_artifact(dir.path(), "original", b"content");
        let expected = sha_file(&original);

        let dst = dir.path().join("out");
        let pool = WriterPool::new();
        let res = extract_patch_tar_gz_parallel(&artifact, &dst, profile(), &pool, true, &expected);
        assert!(res.is_err(), "corrupt artifact must fail closed");
    }
}
