//! In-process Mach-O relocation: magic sniffing, load-command walking, and
//! in-place rewriting of install-name / dylib-id / rpath strings.
//!
//! Port of the Homebrew bottle-install semantics from
//! `extend/os/mac/keg_relocate.rb` (`relocate_dynamic_linkage` →
//! `relocated_name_for`) with the in-place mechanics of the vendored
//! `ruby-macho` gem's string replacement (growth via the header pad,
//! `HeaderPadError` when it is exhausted). See
//! `docs/explanation/install-pipeline.md` for the strategy and fidelity notes.

use anyhow::{bail, Result};
use memchr::memmem;

#[derive(Debug, Default, Clone, Copy)]
pub struct PatchCounts {
    pub string_replacements: usize,
}

// Mach-O magic numbers (as stored on disk).
const MH_MAGIC: u32 = 0xfeedface;
const MH_CIGAM: u32 = 0xcefaedfe;
const MH_MAGIC_64: u32 = 0xfeedfacf;
const MH_CIGAM_64: u32 = 0xcffaedfe;

/// Filetypes Homebrew relocates (`mach_o_files` in
/// `extend/os/mac/keg_relocate.rb`): dylib, bundle, executable. Everything
/// else (e.g. `MH_OBJECT`) is left alone.
const MH_EXECUTE: u32 = 0x2;
const MH_DYLIB: u32 = 0x6;
const MH_BUNDLE: u32 = 0x8;

// Load commands with a single string payload that Homebrew relocates
// (`relocate_dynamic_linkage`: LC_ID_DYLIB for dylibs, the DYLIB_LOAD_COMMANDS
// family, and LC_RPATH). LC_LOAD_DYLINKER / LC_DYLD_ENVIRONMENT are not
// relocated by Homebrew and are deliberately absent here.
const LC_LOAD_DYLIB: u32 = 0x0000_000c;
const LC_ID_DYLIB: u32 = 0x0000_000d;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LAZY_LOAD_DYLIB: u32 = 0x0000_0020;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_UUID: u32 = 0x0000_001b;
const LC_SEGMENT: u32 = 0x0000_0001;
const LC_SEGMENT_64: u32 = 0x0000_0019;

/// Parsed thin Mach-O header layout.
#[derive(Debug, Clone, Copy)]
pub struct HeaderLayout {
    pub header_size: usize,
    pub sizeofcmds: usize,
    pub little: bool,
}

/// Parse a thin Mach-O header, matching ruby-macho's `populate_and_check_magic`
/// (macho_file.rb): the magic is read big-endian; little-endian files carry the
/// CIGAM byte pattern on disk (i386.dylib starts `ce fa ed fe`, x86_64.dylib
/// `cf fa ed fe`) while big-endian files carry the plain MH_MAGIC bytes, so
/// CIGAM -> little-endian fields, MH_MAGIC -> big-endian fields. Returns None
/// for non-Mach-O data and fat files.
pub fn header_layout(data: &[u8]) -> Option<HeaderLayout> {
    let magic = read_u32_be(data, 0)?;
    let (little, header_size) = match magic {
        MH_CIGAM => (true, 28),
        MH_CIGAM_64 => (true, 32),
        MH_MAGIC => (false, 28),
        MH_MAGIC_64 => (false, 32),
        _ => return None,
    };
    let sizeofcmds = read_u32_e(data, 20, little)? as usize;
    Some(HeaderLayout {
        header_size,
        sizeofcmds,
        little,
    })
}

/// Ad-hoc signing identifier for a thin Mach-O slice, matching ruby-macho's
/// `CodeSigning.identifier` (code_signing.rb): first an embedded Info.plist
/// `CFBundleIdentifier`, then a file stem containing a dot used as-is, then
/// `"<stem>-<hex>"` where hex is `"UUID"` + the LC_UUID bytes or, for legacy
/// inputs without a UUID, SHA-1 of the first 28 bytes (`MachHeader.bytesize`)
/// plus the load-command region. `region` must contain at least the header and
/// all load commands; `info_plist` is the `__TEXT,__info_plist` section content
/// (the caller reads it, since it may live beyond the load-command region).
pub fn signing_identifier(region: &[u8], stem: &str, info_plist: Option<&[u8]>) -> Option<String> {
    let layout = header_layout(region)?;
    if let Some(id) = plist_cfbundle_identifier(info_plist) {
        return Some(id);
    }
    if stem.contains('.') {
        return Some(stem.to_string());
    }
    let ncmds = read_u32_e(region, 16, layout.little)? as usize;
    let mut off = layout.header_size;
    let mut identity = None;
    for _ in 0..ncmds {
        let (Some(cmd), Some(cmdsize)) = (
            read_u32_e(region, off, layout.little),
            read_u32_e(region, off + 4, layout.little),
        ) else {
            break;
        };
        let cmdsize = cmdsize as usize;
        if cmdsize < 8
            || off
                .checked_add(cmdsize)
                .map(|end| end > region.len())
                .unwrap_or(true)
        {
            break;
        }
        if cmd == LC_UUID && cmdsize >= 24 {
            if let Some(uuid) = region.get(off + 8..off + 24) {
                let mut id = b"UUID".to_vec();
                id.extend_from_slice(uuid);
                identity = Some(crate::hash::hex_lower(&id));
                break;
            }
        }
        off += cmdsize;
    }
    let identity = identity.unwrap_or_else(|| {
        // Legacy: SHA-1 of the first 28 bytes plus the load-command region.
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        ctx.update(region.get(..28).unwrap_or(region));
        if let Some(load_cmds) =
            region.get(layout.header_size..layout.header_size + layout.sizeofcmds)
        {
            ctx.update(load_cmds);
        }
        crate::hash::hex_lower(ctx.finish().as_ref())
    });
    Some(format!("{stem}-{identity}"))
}

/// File range (offset, size) of the section named `sectname` in segment
/// `segname` within the load-command region — e.g. `__TEXT,__info_plist`,
/// mirroring ruby-macho's `CodeSigning.info_plist` section search
/// (code_signing.rb) over `LC_SEGMENT`/`LC_SEGMENT_64`.
pub fn section_range(
    region: &[u8],
    layout: HeaderLayout,
    segname: &[u8],
    sectname: &[u8],
) -> Option<(usize, usize)> {
    let ncmds = read_u32_e(region, 16, layout.little)? as usize;
    let mut off = layout.header_size;
    for _ in 0..ncmds {
        let (Some(cmd), Some(cmdsize)) = (
            read_u32_e(region, off, layout.little),
            read_u32_e(region, off + 4, layout.little),
        ) else {
            break;
        };
        let cmdsize = cmdsize as usize;
        if cmdsize < 8
            || off
                .checked_add(cmdsize)
                .map(|end| end > region.len())
                .unwrap_or(true)
        {
            break;
        }
        let is_64 = cmd == LC_SEGMENT_64;
        let is_32 = cmd == LC_SEGMENT;
        if is_64 || is_32 {
            // segment_command: segname at +8 (16 bytes), nsects at +64 (64-bit)
            // or +48 (32-bit); sections follow the 72/56-byte fixed header.
            let (nsects_off, section_base, section_entry_size) =
                if is_64 { (64, 72, 80) } else { (48, 56, 68) };
            let seg = trimmed_name(region.get(off + 8..off + 24)?);
            let nsects = read_u32_e(region, off + nsects_off, layout.little)? as usize;
            let mut s = off + section_base;
            for _ in 0..nsects {
                let sect = trimmed_name(region.get(s..s + 16)?);
                let (offset_off, size_off) = if is_64 { (32, 40) } else { (24, 28) };
                let section_offset = read_u32_e(region, s + offset_off, layout.little)? as usize;
                let section_size = read_u32_e(region, s + size_off, layout.little)? as usize;
                if seg == segname && sect == sectname {
                    return Some((section_offset, section_size));
                }
                s += section_entry_size;
            }
        }
        off += cmdsize;
    }
    None
}

/// `CFBundleIdentifier` from an embedded Info.plist, approximating ruby-macho's
/// regex `%r{<key>\s*CFBundleIdentifier\s*</key>\s*<string>\s*([^<]+?)\s*</string>}m`.
fn plist_cfbundle_identifier(plist: Option<&[u8]>) -> Option<String> {
    let plist = plist?;
    let mut offset = 0;
    while let Some(start) = memmem::find(&plist[offset..], b"CFBundleIdentifier") {
        let start = offset + start;
        let after_key =
            memmem::find(&plist[start..], b"</key>").map(|p| start + p + b"</key>".len());
        if let Some(after_key) = after_key {
            if let Some(str_start) = memmem::find(&plist[after_key..], b"<string>")
                .map(|p| after_key + p + b"<string>".len())
            {
                if let Some(str_end) =
                    memmem::find(&plist[str_start..], b"</string>").map(|p| str_start + p)
                {
                    let value = std::str::from_utf8(&plist[str_start..str_end])
                        .unwrap_or("")
                        .trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
        offset = start + b"CFBundleIdentifier".len();
    }
    None
}

/// Trim a nul-padded load-command name field.
fn trimmed_name(field: &[u8]) -> &[u8] {
    match field.iter().position(|b| *b == 0) {
        Some(pos) => &field[..pos],
        None => field,
    }
}

/// Homebrew `relocated_name_for` (`extend/os/mac/keg_relocate.rb`): only the
/// CELLAR then PREFIX placeholder at the *start* of the string is replaced.
/// The repository/library/perl placeholders are text-relocation concerns and
/// are never applied to load-command strings.
fn relocated_string(value: &str, prefix: &str) -> Option<String> {
    if let Some(rest) = value.strip_prefix("@@HOMEBREW_CELLAR@@") {
        return Some(format!("{prefix}/Cellar{rest}"));
    }
    if let Some(rest) = value.strip_prefix("@@HOMEBREW_PREFIX@@") {
        return Some(format!("{prefix}{rest}"));
    }
    None
}

/// Relocate a Mach-O file's load-command strings in place, mirroring Homebrew's
/// `relocate_dynamic_linkage` (extend/os/mac/keg_relocate.rb) for the
/// bottle-pour case: dylib IDs, install names, and rpaths get the
/// `relocated_name_for` treatment; fat files get every slice patched. Growth
/// is handled in-process via the header pad (like ruby-macho); when the pad is
/// exhausted the prepare fails (like Homebrew's `HeaderPadError` → pour
/// failure). No external tool is involved.
///
/// Returns `(is_macho, mutated)`:
/// - `is_macho`: the file has Mach-O magic (any filetype). Callers use this to
///   decide whether to ad-hoc sign and to skip the text-relocation pass.
/// - `mutated`: at least one load-command string was rewritten.
///
/// All string reads are bounds-checked; malformed files produce warnings and
/// stop the walk rather than panicking.
pub fn patch_macho(
    rel: &str,
    data: &mut Vec<u8>,
    prefix: &str,
    counts: &mut PatchCounts,
    warnings: &mut Vec<String>,
) -> Result<(bool, bool)> {
    match data.get(0..4) {
        Some([0xca, 0xfe, 0xba, 0xbe]) if plausible_fat_header(data, false) => {
            Ok((true, patch_fat(rel, data, false, prefix, counts, warnings)?))
        }
        Some([0xca, 0xfe, 0xba, 0xbf]) if plausible_fat_header(data, true) => {
            Ok((true, patch_fat(rel, data, true, prefix, counts, warnings)?))
        }
        Some(
            [0xfe, 0xed, 0xfa, 0xce]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xcf, 0xfa, 0xed, 0xfe],
        ) => Ok((true, patch_slice(rel, 0, data, prefix, counts, warnings)?)),
        _ => Ok((false, false)),
    }
}

/// Cheap Mach-O magic/header sniff used when another relocation pass already
/// decided a file changed and the caller only needs to know whether ad-hoc
/// signing is required. This intentionally mirrors `patch_macho`'s acceptance
/// of thin files plus plausible fat headers, without walking load commands.
pub fn is_macho(data: &[u8]) -> bool {
    match data.get(0..4) {
        Some([0xca, 0xfe, 0xba, 0xbe]) => plausible_fat_header(data, false),
        Some([0xca, 0xfe, 0xba, 0xbf]) => plausible_fat_header(data, true),
        Some(
            [0xfe, 0xed, 0xfa, 0xce]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xcf, 0xfa, 0xed, 0xfe],
        ) => true,
        _ => false,
    }
}

/// Set a dylib's install ID (the `LC_ID_DYLIB` string) in place, mirroring
/// ruby-macho's in-process `MachOFile#change_dylib_id` (macho_file.rb) — the
/// zero-dependency replacement for the structured-postinstall step. Grows via
/// the header pad when needed; errors like Homebrew's `HeaderPadError` when
/// the pad is exhausted.
pub fn set_dylib_id(data: &mut Vec<u8>, new_id: &str) -> Result<()> {
    match data.get(0..4) {
        Some([0xca, 0xfe, 0xba, 0xbe]) if plausible_fat_header(data, false) => {
            set_dylib_id_fat(data, false, new_id)
        }
        Some([0xca, 0xfe, 0xba, 0xbf]) if plausible_fat_header(data, true) => {
            set_dylib_id_fat(data, true, new_id)
        }
        Some(
            [0xfe, 0xed, 0xfa, 0xce]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xcf, 0xfa, 0xed, 0xfe],
        ) => set_dylib_id_thin(0, data, new_id),
        _ => bail!("not a Mach-O file"),
    }
}

fn set_dylib_id_fat(data: &mut Vec<u8>, is_64: bool, new_id: &str) -> Result<()> {
    let Some(nfat) = read_u32_be(data, 4) else {
        bail!("truncated fat header");
    };
    let arch_size = if is_64 { 32 } else { 20 };
    for i in 0..nfat as usize {
        let base = 8 + i * arch_size;
        let (Some(offset), Some(size)) = (
            read_u64_or_u32_be(data, base + 8, is_64),
            read_u64_or_u32_be(data, base + if is_64 { 16 } else { 12 }, is_64),
        ) else {
            continue;
        };
        let offset = offset as usize;
        let size = size as usize;
        if offset
            .checked_add(size)
            .map(|end| end <= data.len())
            .unwrap_or(false)
        {
            set_dylib_id_thin(offset, data, new_id)?;
        }
    }
    Ok(())
}

/// Iterate the architecture slices of a fat binary (32- or 64-bit arch table)
/// and relocate each in place. Growth via the header pad is size-preserving
/// (the pad tail is drained to keep the slice length constant), so slice
/// offsets/sizes in the fat arch table never change.
fn patch_fat(
    rel: &str,
    data: &mut Vec<u8>,
    is_64: bool,
    prefix: &str,
    counts: &mut PatchCounts,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let mut mutated = false;
    let Some(nfat) = read_u32_be(data, 4) else {
        return Ok(false);
    };
    let arch_size = if is_64 { 32 } else { 20 };
    for i in 0..nfat as usize {
        let base = 8 + i * arch_size;
        let (Some(offset), Some(size)) = (
            read_u64_or_u32_be(data, base + 8, is_64),
            read_u64_or_u32_be(data, base + if is_64 { 16 } else { 12 }, is_64),
        ) else {
            continue;
        };
        let offset = offset as usize;
        let size = size as usize;
        if offset
            .checked_add(size)
            .map(|end| end <= data.len())
            .unwrap_or(false)
        {
            mutated |= patch_slice(rel, offset, data, prefix, counts, warnings)?;
        }
    }
    Ok(mutated)
}

/// One slice's parsed header layout plus its absolute position in the file.
struct SliceInfo {
    base: usize,
    size: usize,
    header_size: usize,
    little: bool,
    alignment: usize,
}

/// A pending in-place load-command string rewrite.
struct StringRewrite {
    cmd_offset: usize,  // absolute file offset of the command
    name_offset: usize, // lc_str offset relative to the command
    new: String,
}

/// Relocate one thin Mach-O slice: walk its load commands and rewrite the
/// string payloads of the commands Homebrew relocates.
fn patch_slice(
    rel: &str,
    base: usize,
    data: &mut Vec<u8>,
    prefix: &str,
    counts: &mut PatchCounts,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let Some(info) = slice_info(data, base) else {
        return Ok(false);
    };
    let Some(filetype) = read_u32_e(data, base + 12, info.little) else {
        return Ok(false);
    };
    if !matches!(filetype, MH_EXECUTE | MH_DYLIB | MH_BUNDLE) {
        // Homebrew's mach_o_files skips objects, kernels, etc.
        return Ok(false);
    }
    // LC_ID_DYLIB is only rewritten for dylibs (Homebrew gates on file.dylib?).
    let patch_id = filetype == MH_DYLIB;

    let mut rewrites = Vec::new();
    let mut mutated = false;
    walk_string_commands(
        rel,
        data,
        &info,
        warnings,
        |cmd, cmd_offset, name_offset, value| {
            if cmd != LC_ID_DYLIB || patch_id {
                if let Some(new) = relocated_string(value, prefix) {
                    if new != value {
                        rewrites.push(StringRewrite {
                            cmd_offset,
                            name_offset,
                            new,
                        });
                    }
                }
            }
        },
    );
    apply_rewrites(rel, data, &info, &rewrites, counts, &mut mutated)?;
    Ok(mutated)
}

/// Set the LC_ID_DYLIB string of a thin slice to `new_id` (see `set_dylib_id`).
fn set_dylib_id_thin(base: usize, data: &mut Vec<u8>, new_id: &str) -> Result<()> {
    let Some(info) = slice_info(data, base) else {
        bail!("not a thin Mach-O");
    };
    let mut rewrites = Vec::new();
    let mut walk_warnings = Vec::new();
    walk_string_commands(
        "",
        data,
        &info,
        &mut walk_warnings,
        |cmd, cmd_offset, name_offset, _value| {
            if cmd == LC_ID_DYLIB {
                rewrites.push(StringRewrite {
                    cmd_offset,
                    name_offset,
                    new: new_id.to_string(),
                });
            }
        },
    );
    let mut mutated = false;
    let mut counts = PatchCounts::default();
    apply_rewrites("", data, &info, &rewrites, &mut counts, &mut mutated)?;
    if !mutated {
        bail!("no LC_ID_DYLIB load command found");
    }
    Ok(())
}

/// Parse a slice's header into `SliceInfo`, or None if it is not a thin Mach-O.
fn slice_info(data: &[u8], base: usize) -> Option<SliceInfo> {
    let layout = header_layout(data.get(base..base + 24)?)?;
    let alignment = if layout.header_size == 32 { 8 } else { 4 };
    Some(SliceInfo {
        base,
        size: data.len() - base,
        header_size: layout.header_size,
        little: layout.little,
        alignment,
    })
}

/// Walk a slice's string load commands, calling `visit` with the command, its
/// absolute offset, the lc_str name offset, and the current string value.
/// Bounds-checked; malformed commands stop the walk silently.
fn walk_string_commands(
    rel: &str,
    data: &[u8],
    info: &SliceInfo,
    warnings: &mut Vec<String>,
    mut visit: impl FnMut(u32, usize, usize, &str),
) {
    let base = info.base;
    let Some(ncmds) = read_u32_e(data, base + 16, info.little) else {
        return;
    };
    let mut off = base + info.header_size;
    for _ in 0..ncmds {
        let (Some(cmd), Some(cmdsize)) = (
            read_u32_e(data, off, info.little),
            read_u32_e(data, off + 4, info.little),
        ) else {
            break;
        };
        let cmdsize = cmdsize as usize;
        if cmdsize < 8
            || off
                .checked_add(cmdsize)
                .map(|end| end > data.len())
                .unwrap_or(true)
        {
            warnings.push(format!(
                "invalid Mach-O load command in {rel}: slice_offset={base} offset={off} size={cmdsize}"
            ));
            break;
        }
        if is_relocatable_string_command(cmd) {
            if let Some(name_offset) = read_u32_e(data, off + 8, info.little) {
                let str_start = off + name_offset as usize;
                let str_end = off + cmdsize;
                if str_start < str_end && str_end <= data.len() {
                    let nul = data[str_start..str_end]
                        .iter()
                        .position(|b| *b == 0)
                        .unwrap_or(str_end - str_start);
                    if let Ok(value) = std::str::from_utf8(&data[str_start..str_start + nul]) {
                        visit(cmd, off, name_offset as usize, value);
                    }
                }
            }
        }
        off += cmdsize;
    }
}

/// Apply collected rewrites: zero-fill (or absorb into existing padding) where
/// the new string fits, grow via the header pad where it is longer, and fail
/// like Homebrew's `HeaderPadError` when the pad is exhausted. Rewrites are
/// applied in reverse command order so earlier commands' recorded offsets stay
/// valid; the pad tail is drained to keep the file size constant.
fn apply_rewrites(
    rel: &str,
    data: &mut Vec<u8>,
    info: &SliceInfo,
    rewrites: &[StringRewrite],
    counts: &mut PatchCounts,
    mutated: &mut bool,
) -> Result<()> {
    if rewrites.is_empty() {
        return Ok(());
    }
    let region = &data[info.base..info.base + info.size];
    let low = low_fileoff(region, info.header_size, info.little);
    let mut sizeofcmds = read_u32_e(data, info.base + 20, info.little).unwrap_or(0) as usize;

    for fixup in rewrites.iter().rev() {
        let str_start = fixup.cmd_offset + fixup.name_offset;
        let old_cmdsize = read_u32_e(data, fixup.cmd_offset + 4, info.little).unwrap_or(0) as usize;
        let new_cmdsize = align_up(fixup.name_offset + fixup.new.len() + 1, info.alignment);
        let delta = new_cmdsize as isize - old_cmdsize as isize;

        if delta > 0 {
            let delta = delta as usize;
            // HeaderPadError analog: the load-command region plus the delta must
            // fit before the first segment/section data.
            if info.base + info.header_size + sizeofcmds + delta > info.base + low {
                bail!(
                    "cannot grow load command in {rel}: header pad exhausted (needs {delta}+ bytes past {low}) — same failure as Homebrew's HeaderPadError"
                );
            }
            // Rebuild the command: preserve fixed fields verbatim (which also
            // covers the macOS-15 DylibUseCommand flags field), update cmdsize,
            // then the new string + NUL + alignment padding.
            let mut cmd = Vec::with_capacity(new_cmdsize);
            cmd.extend_from_slice(&data[fixup.cmd_offset..str_start]);
            write_u32_e(&mut cmd, 4, new_cmdsize as u32, info.little);
            cmd.extend_from_slice(fixup.new.as_bytes());
            cmd.push(0);
            cmd.resize(new_cmdsize, 0);
            data.splice(fixup.cmd_offset..fixup.cmd_offset + old_cmdsize, cmd);
            // Drain the same number of bytes from the pad tail to keep the file
            // size constant, so segment/section offsets never change.
            let pad_tail = info.base + info.header_size + sizeofcmds + delta;
            data.drain(pad_tail..pad_tail + delta);
            sizeofcmds += delta;
            write_u32_e(data, info.base + 20, sizeofcmds as u32, info.little);
        } else {
            // Fits (shrink or absorbed by existing padding): zero the whole old
            // command's string slot and copy, leaving cmdsize and sizeofcmds
            // untouched.
            let cmd_end = fixup.cmd_offset + old_cmdsize;
            data[str_start..cmd_end].fill(0);
            data[str_start..str_start + fixup.new.len()].copy_from_slice(fixup.new.as_bytes());
        }
        counts.string_replacements += 1;
        *mutated = true;
    }
    Ok(())
}

/// First data offset after the load commands (the header pad extends to here),
/// mirroring ruby-macho's `MachOFile#low_fileoff` (macho_file.rb): min over
/// zero-section segment fileoffs and non-zerofill section offsets.
fn low_fileoff(region: &[u8], header_size: usize, little: bool) -> usize {
    let mut low = region.len();
    let ncmds = read_u32_e(region, 16, little).unwrap_or(0) as usize;
    let mut off = header_size;
    for _ in 0..ncmds {
        let (Some(cmd), Some(cmdsize)) = (
            read_u32_e(region, off, little),
            read_u32_e(region, off + 4, little),
        ) else {
            break;
        };
        let cmdsize = cmdsize as usize;
        if cmdsize < 8
            || off
                .checked_add(cmdsize)
                .map(|end| end > region.len())
                .unwrap_or(true)
        {
            break;
        }
        let is_64 = cmd == LC_SEGMENT_64;
        let is_32 = cmd == LC_SEGMENT;
        if is_64 || is_32 {
            let (nsects_off, section_base, section_entry) =
                if is_64 { (64, 72, 80) } else { (48, 56, 68) };
            let (fileoff, filesize) = if is_64 {
                (
                    read_u64_e(region, off + 32, little).unwrap_or(0) as usize,
                    read_u64_e(region, off + 40, little).unwrap_or(0) as usize,
                )
            } else {
                (
                    read_u32_e(region, off + 24, little).unwrap_or(0) as usize,
                    read_u32_e(region, off + 28, little).unwrap_or(0) as usize,
                )
            };
            let nsects = read_u32_e(region, off + nsects_off, little).unwrap_or(0) as usize;
            if nsects == 0 && fileoff > 0 && filesize > 0 && fileoff < low {
                low = fileoff;
            }
            let (sect_off_off, sect_size_off, sect_flags_off) =
                if is_64 { (32, 40, 56) } else { (24, 28, 48) };
            let mut s = off + section_base;
            for _ in 0..nsects {
                let sect_size = read_u32_e(region, s + sect_size_off, little).unwrap_or(0) as usize;
                let sect_flags = read_u32_e(region, s + sect_flags_off, little).unwrap_or(0);
                // Skip empty and zerofill sections (S_ZEROFILL, S_THREAD_LOCAL_ZEROFILL).
                if sect_size > 0 && (sect_flags & 0xff) != 0x1 && (sect_flags & 0xff) != 0x12 {
                    if let Some(sect_off) = read_u32_e(region, s + sect_off_off, little) {
                        if (sect_off as usize) < low {
                            low = sect_off as usize;
                        }
                    }
                }
                s += section_entry;
            }
        }
        off += cmdsize;
    }
    low
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn write_u32_e(data: &mut [u8], offset: usize, value: u32, little: bool) {
    let bytes = if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    if let Some(slot) = data.get_mut(offset..offset + 4) {
        slot.copy_from_slice(&bytes);
    }
}

/// Whether this load command carries the kind of string Homebrew relocates
/// (Homebrew's `DYLIB_LOAD_COMMANDS` + LC_ID_DYLIB + LC_RPATH, as walked by
/// `relocate_dynamic_linkage` in extend/os/mac/keg_relocate.rb).
fn is_relocatable_string_command(cmd: u32) -> bool {
    matches!(
        cmd,
        LC_LOAD_DYLIB
            | LC_ID_DYLIB
            | LC_LOAD_WEAK_DYLIB
            | LC_REEXPORT_DYLIB
            | LC_LOAD_UPWARD_DYLIB
            | LC_LAZY_LOAD_DYLIB
            | LC_RPATH
    )
}

/// Fat-header sanity check mirroring ruby-macho's `FatFile`/`FatArch` layout
/// (fat_file.rb): `cafebabe` + nfat + 20-byte arch entries, or the 64-bit
/// `cafebf` variant with 32-byte entries.
fn plausible_fat_header(data: &[u8], is_64: bool) -> bool {
    let Some(nfat) = read_u32_be(data, 4) else {
        return false;
    };
    if nfat == 0 || nfat > 64 {
        return false;
    }
    let arch_size = if is_64 { 32 } else { 20 };
    8usize
        .checked_add(nfat as usize * arch_size)
        .map(|need| need <= data.len())
        .unwrap_or(false)
}

fn read_u32_e(data: &[u8], offset: usize, little: bool) -> Option<u32> {
    if little {
        read_u32_le(data, offset)
    } else {
        read_u32_be(data, offset)
    }
}

fn read_u64_e(data: &[u8], offset: usize, little: bool) -> Option<u64> {
    if little {
        Some(u64::from_le_bytes(
            data.get(offset..offset + 8)?.try_into().ok()?,
        ))
    } else {
        Some(u64::from_be_bytes(
            data.get(offset..offset + 8)?.try_into().ok()?,
        ))
    }
}

fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u32_be(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64_be(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_u64_or_u32_be(data: &[u8], offset: usize, is_64: bool) -> Option<u64> {
    if is_64 {
        read_u64_be(data, offset)
    } else {
        read_u32_be(data, offset).map(u64::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    fn u32be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn align8(n: usize) -> usize {
        (n + 7) & !7
    }

    fn align4(n: usize) -> usize {
        (n + 3) & !3
    }

    /// A dylib-family string command (LC_ID_DYLIB / LC_LOAD_DYLIB family):
    /// cmd, cmdsize, name offset at +8, then 12 bytes of version fields, then
    /// the nul-terminated string, padded so cmdsize is 8-aligned.
    fn dylib_cmd64(cmd: u32, s: &str) -> Vec<u8> {
        let fixed = 24usize;
        let cmdsize = align8(fixed + s.len() + 1);
        let mut v = Vec::with_capacity(cmdsize);
        v.extend_from_slice(&u32le(cmd));
        v.extend_from_slice(&u32le(cmdsize as u32));
        v.extend_from_slice(&u32le(fixed as u32)); // name offset
        v.resize(fixed, 0);
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        v.resize(cmdsize, 0);
        v
    }

    /// LC_RPATH: cmd, cmdsize, path offset at +8, then the string.
    fn rpath_cmd64(s: &str) -> Vec<u8> {
        let fixed = 12usize;
        let cmdsize = align8(fixed + s.len() + 1);
        let mut v = Vec::with_capacity(cmdsize);
        v.extend_from_slice(&u32le(LC_RPATH));
        v.extend_from_slice(&u32le(cmdsize as u32));
        v.extend_from_slice(&u32le(fixed as u32)); // path offset
        v.resize(fixed, 0);
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        v.resize(cmdsize, 0);
        v
    }

    /// 64-bit little-endian Mach-O header + commands.
    fn macho64(filetype: u32, commands: &[Vec<u8>]) -> Vec<u8> {
        let header = 32usize;
        let sizeofcmds: usize = commands.iter().map(|c| c.len()).sum();
        let mut v = Vec::new();
        v.extend_from_slice(&u32le(MH_MAGIC_64));
        v.extend_from_slice(&u32le(0x0100_000c)); // cputype arm64
        v.extend_from_slice(&u32le(0)); // cpusubtype
        v.extend_from_slice(&u32le(filetype));
        v.extend_from_slice(&u32le(commands.len() as u32)); // ncmds
        v.extend_from_slice(&u32le(sizeofcmds as u32)); // sizeofcmds
        v.extend_from_slice(&u32le(0)); // flags
        v.extend_from_slice(&u32le(0)); // reserved
        for c in commands {
            v.extend_from_slice(c);
        }
        debug_assert_eq!(v.len(), header + sizeofcmds);
        v
    }

    /// Fat (32-bit arch table) wrapper around thin slices.
    fn fat32(slices: &[Vec<u8>]) -> Vec<u8> {
        let header = 8 + slices.len() * 20;
        let mut v = Vec::new();
        v.extend_from_slice(&u32be(0xcafe_babe));
        v.extend_from_slice(&u32be(slices.len() as u32));
        let mut offset = header;
        for (i, slice) in slices.iter().enumerate() {
            let cputype = if i == 0 { 0x0100_000c } else { 0x0100_0007 };
            v.extend_from_slice(&u32be(cputype));
            v.extend_from_slice(&u32be(0)); // cpusubtype
            v.extend_from_slice(&u32be(offset as u32));
            v.extend_from_slice(&u32be(slice.len() as u32));
            v.extend_from_slice(&u32be(4)); // align = 2^4
            offset += slice.len();
        }
        for slice in slices {
            v.extend_from_slice(slice);
        }
        v
    }

    /// Offset of the idx-th load command (64-bit LE header assumed).
    fn cmd_offset64(data: &[u8], idx: usize) -> usize {
        let ncmds = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
        assert!(idx < ncmds, "idx {idx} >= ncmds {ncmds}");
        let mut off = 32;
        for _ in 0..idx {
            let cmdsize = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
            off += cmdsize;
        }
        off
    }

    /// Read the string payload of a command at `cmd_offset` (dylib/rpath
    /// layouts, 64-bit LE).
    fn cmd_string64(data: &[u8], cmd_offset: usize) -> String {
        let name_off =
            u32::from_le_bytes(data[cmd_offset + 8..cmd_offset + 12].try_into().unwrap()) as usize;
        let cmdsize =
            u32::from_le_bytes(data[cmd_offset + 4..cmd_offset + 8].try_into().unwrap()) as usize;
        let start = cmd_offset + name_off;
        let end = cmd_offset + cmdsize;
        let nul = data[start..end].iter().position(|b| *b == 0).unwrap();
        String::from_utf8(data[start..start + nul].to_vec()).unwrap()
    }

    /// The bytes between the string and the end of its command must be zero
    /// (zero-fill of a shrunk command).
    fn assert_tail_zeroed(data: &[u8], cmd_offset: usize, string_len: usize) {
        let name_off =
            u32::from_le_bytes(data[cmd_offset + 8..cmd_offset + 12].try_into().unwrap()) as usize;
        let cmdsize =
            u32::from_le_bytes(data[cmd_offset + 4..cmd_offset + 8].try_into().unwrap()) as usize;
        let start = cmd_offset + name_off;
        assert!(
            data[start + string_len..cmd_offset + cmdsize]
                .iter()
                .all(|b| *b == 0),
            "bytes after the string must be zeroed"
        );
    }

    fn patch(data: &mut Vec<u8>, prefix: &str) -> (bool, bool, PatchCounts, Vec<String>) {
        let mut counts = PatchCounts::default();
        let mut warnings = Vec::new();
        let (is_macho, mutated) =
            patch_macho("test", data, prefix, &mut counts, &mut warnings).expect("patch_macho");
        (is_macho, mutated, counts, warnings)
    }

    #[test]
    fn relocates_prefix_placeholder_in_place() {
        let prefix = "/opt/glustore";
        let id = "@@HOMEBREW_PREFIX@@/Cellar/fixture/1.0/lib/libfixture.dylib";
        let load = "@@HOMEBREW_PREFIX@@/opt/dep/lib/libdep.dylib";
        let rpath = "@@HOMEBREW_PREFIX@@/lib";
        let mut data = macho64(
            MH_DYLIB,
            &[
                dylib_cmd64(LC_ID_DYLIB, id),
                dylib_cmd64(LC_LOAD_DYLIB, load),
                rpath_cmd64(rpath),
            ],
        );
        let (is_macho, mutated, counts, warnings) = patch(&mut data, prefix);
        assert!(is_macho);
        assert!(mutated);
        assert_eq!(counts.string_replacements, 3);
        assert!(warnings.is_empty());

        let id_off = cmd_offset64(&data, 0);
        let load_off = cmd_offset64(&data, 1);
        let rpath_off = cmd_offset64(&data, 2);
        assert_eq!(
            cmd_string64(&data, id_off),
            "/opt/glustore/Cellar/fixture/1.0/lib/libfixture.dylib"
        );
        assert_eq!(
            cmd_string64(&data, load_off),
            "/opt/glustore/opt/dep/lib/libdep.dylib"
        );
        assert_eq!(cmd_string64(&data, rpath_off), "/opt/glustore/lib");
        // Shrunk commands leave zeroed trailing bytes.
        assert_tail_zeroed(
            &data,
            id_off,
            "/opt/glustore/Cellar/fixture/1.0/lib/libfixture.dylib".len(),
        );
        assert_tail_zeroed(&data, rpath_off, "/opt/glustore/lib".len());
    }

    #[test]
    fn cellar_placeholder_growth_via_header_pad() {
        // 39-char placeholder string -> 41-char result crosses the 8-byte alignment
        // boundary: cmdsize grows 64 -> 72, absorbed from the header pad in-process
        // (ruby-macho semantics). File length stays constant, sizeofcmds grows.
        // 39-char id -> 41-char result: cmdsize 64 -> 72 (delta 8), absorbed
        // from the header pad in-process (ruby-macho semantics). File length
        // stays constant, sizeofcmds grows by 8.
        let id = "@@HOMEBREW_CELLAR@@/0123456789012345678";
        let expected = "/opt/glustore/Cellar/0123456789012345678";
        let mut data = macho64(MH_DYLIB, &[dylib_cmd64(LC_ID_DYLIB, id)]);
        let len_before = data.len();
        let sizeofcmds_before = u32::from_le_bytes(data[20..24].try_into().unwrap());
        data.extend_from_slice(&[0u8; 64]); // header pad after the load commands
        let (is_macho, mutated, counts, warnings) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(mutated);
        assert_eq!(counts.string_replacements, 1);
        assert!(warnings.is_empty());
        assert_eq!(data.len(), len_before + 64, "file length must be preserved");
        assert_eq!(cmd_string64(&data, cmd_offset64(&data, 0)), expected);
        let sizeofcmds_after = u32::from_le_bytes(data[20..24].try_into().unwrap());
        assert_eq!(
            sizeofcmds_after,
            sizeofcmds_before + 8,
            "sizeofcmds must grow by the delta"
        );
    }

    #[test]
    fn cellar_placeholder_growth_pad_exhausted_fails() {
        // No header pad: growth must fail like Homebrew's HeaderPadError, with no
        // external tool involved.
        let id = "@@HOMEBREW_CELLAR@@/0123456789012345678";
        let mut data = macho64(MH_DYLIB, &[dylib_cmd64(LC_ID_DYLIB, id)]);
        let mut counts = PatchCounts::default();
        let mut warnings = Vec::new();
        let err = patch_macho(
            "test",
            &mut data,
            "/opt/glustore",
            &mut counts,
            &mut warnings,
        )
        .unwrap_err();
        assert!(err.to_string().contains("pad exhausted"));
    }

    #[test]
    fn id_only_rewritten_for_dylibs() {
        // An executable with a (synthetic) LC_ID_DYLIB: Homebrew only rewrites
        // the ID when file.dylib? is true, so the ID must be left alone while
        // a sibling LC_LOAD_DYLIB is still relocated.
        let id = "@@HOMEBREW_PREFIX@@/Cellar/fixture/1.0/lib/libfixture.dylib";
        let load = "@@HOMEBREW_PREFIX@@/opt/dep/lib/libdep.dylib";
        let mut data = macho64(
            MH_EXECUTE,
            &[
                dylib_cmd64(LC_ID_DYLIB, id),
                dylib_cmd64(LC_LOAD_DYLIB, load),
            ],
        );
        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(mutated);
        assert_eq!(counts.string_replacements, 1);
        assert_eq!(
            cmd_string64(&data, cmd_offset64(&data, 0)),
            id,
            "LC_ID_DYLIB must not be rewritten for non-dylibs"
        );
        assert_eq!(
            cmd_string64(&data, cmd_offset64(&data, 1)),
            "/opt/glustore/opt/dep/lib/libdep.dylib"
        );
    }

    #[test]
    fn placeholder_not_at_string_start_is_untouched() {
        // Homebrew's relocated_name_for uses start_with?: a placeholder that
        // appears later in the string is not relocated.
        let load = "/lib/@@HOMEBREW_PREFIX@@/x.dylib";
        let mut data = macho64(MH_DYLIB, &[dylib_cmd64(LC_LOAD_DYLIB, load)]);
        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(!mutated);
        assert_eq!(counts.string_replacements, 0);
        assert_eq!(cmd_string64(&data, cmd_offset64(&data, 0)), load);
    }

    #[test]
    fn non_relocatable_filetypes_are_skipped() {
        // MH_OBJECT: magic matches but Homebrew's mach_o_files skips it.
        let load = "@@HOMEBREW_PREFIX@@/opt/dep/lib/libdep.dylib";
        let mut data = macho64(
            0x1, /* MH_OBJECT */
            &[dylib_cmd64(LC_LOAD_DYLIB, load)],
        );
        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho, "still a Mach-O, just not relocatable");
        assert!(!mutated);
        assert_eq!(counts.string_replacements, 0);
        assert_eq!(cmd_string64(&data, cmd_offset64(&data, 0)), load);
    }

    #[test]
    fn dylinker_command_is_not_relocated() {
        // LC_LOAD_DYLINKER is not in Homebrew's relocated set.
        let dylinker = "@@HOMEBREW_PREFIX@@/lib/dyld";
        let mut data = macho64(MH_EXECUTE, &[dylib_cmd64(0x0000_000e, dylinker)]);
        let (is_macho, mutated, _, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(!mutated);
        assert_eq!(cmd_string64(&data, cmd_offset64(&data, 0)), dylinker);
    }

    #[test]
    fn unknown_commands_are_skipped() {
        // An unknown command between two relocatable ones must not derail the
        // walk.
        let id = "@@HOMEBREW_PREFIX@@/Cellar/fixture/1.0/lib/libfixture.dylib";
        let load = "@@HOMEBREW_PREFIX@@/opt/dep/lib/libdep.dylib";
        let unknown = {
            let mut v = vec![0u8; 16];
            v[0..4].copy_from_slice(&u32le(0x1234_5678)); // cmd
            v[4..8].copy_from_slice(&u32le(16)); // cmdsize
            v
        };
        let mut data = macho64(
            MH_DYLIB,
            &[
                dylib_cmd64(LC_ID_DYLIB, id),
                unknown,
                dylib_cmd64(LC_LOAD_DYLIB, load),
            ],
        );
        let (_, mutated, counts, warnings) = patch(&mut data, "/opt/glustore");
        assert!(mutated);
        assert_eq!(counts.string_replacements, 2);
        assert!(warnings.is_empty());
        assert_eq!(
            cmd_string64(&data, cmd_offset64(&data, 0)),
            "/opt/glustore/Cellar/fixture/1.0/lib/libfixture.dylib"
        );
        assert_eq!(
            cmd_string64(&data, cmd_offset64(&data, 2)),
            "/opt/glustore/opt/dep/lib/libdep.dylib"
        );
    }

    #[test]
    fn malformed_command_stops_walk_with_warning() {
        let id = "@@HOMEBREW_PREFIX@@/Cellar/fixture/1.0/lib/libfixture.dylib";
        let bad = {
            let mut v = vec![0u8; 8];
            v[0..4].copy_from_slice(&u32le(LC_LOAD_DYLIB));
            v[4..8].copy_from_slice(&u32le(usize::MAX as u32)); // cmdsize overflows
            v
        };
        let mut data = macho64(MH_DYLIB, &[dylib_cmd64(LC_ID_DYLIB, id), bad]);
        let (_, mutated, counts, warnings) = patch(&mut data, "/opt/glustore");
        assert!(mutated, "the command before the malformed one is patched");
        assert_eq!(counts.string_replacements, 1);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid Mach-O load command"));
    }

    #[test]
    fn fat32_relocates_all_slices() {
        let slice_a = macho64(
            MH_DYLIB,
            &[dylib_cmd64(
                LC_ID_DYLIB,
                "@@HOMEBREW_PREFIX@@/Cellar/a/1.0/lib/liba.dylib",
            )],
        );
        let slice_b = macho64(
            MH_DYLIB,
            &[dylib_cmd64(
                LC_LOAD_DYLIB,
                "@@HOMEBREW_PREFIX@@/opt/b/lib/libb.dylib",
            )],
        );
        let mut data = fat32(&[slice_a, slice_b]);
        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(mutated);
        assert_eq!(counts.string_replacements, 2);
        // Find slice offsets from the arch table and verify each slice.
        let off_a = u32::from_be_bytes(data[8 + 8..8 + 12].try_into().unwrap()) as usize;
        let off_b = u32::from_be_bytes(data[28 + 8..28 + 12].try_into().unwrap()) as usize;
        assert_eq!(
            cmd_string64(&data, off_a + cmd_offset64(&data[off_a..], 0)),
            "/opt/glustore/Cellar/a/1.0/lib/liba.dylib"
        );
        assert_eq!(
            cmd_string64(&data, off_b + cmd_offset64(&data[off_b..], 0)),
            "/opt/glustore/opt/b/lib/libb.dylib"
        );
    }

    #[test]
    fn non_macho_data_is_ignored() {
        let mut data = b"hello world, not a mach-o\0\0\0".to_vec();
        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(!is_macho);
        assert!(!mutated);
        assert_eq!(counts.string_replacements, 0);
    }

    #[test]
    fn big_endian_32bit_slice_is_relocated() {
        // Big-endian 32-bit: magic bytes are the plain MH_MAGIC pattern and
        // fields are big-endian.
        let header = 28usize;
        let id = "@@HOMEBREW_PREFIX@@/Cellar/ppc/1.0/lib/libppc.dylib";
        let fixed = 24usize;
        let cmdsize = align4(fixed + id.len() + 1);
        let mut data = Vec::new();
        data.extend_from_slice(&u32be(MH_MAGIC));
        data.extend_from_slice(&u32be(0x0000_0012)); // cputype ppc
        data.extend_from_slice(&u32be(0)); // cpusubtype
        data.extend_from_slice(&u32be(MH_DYLIB));
        data.extend_from_slice(&u32be(1)); // ncmds
        data.extend_from_slice(&u32be(cmdsize as u32)); // sizeofcmds
        data.extend_from_slice(&u32be(0)); // flags
        data.extend_from_slice(&u32be(LC_ID_DYLIB));
        data.extend_from_slice(&u32be(cmdsize as u32));
        data.extend_from_slice(&u32be(fixed as u32));
        data.resize(header + fixed, 0);
        data.extend_from_slice(id.as_bytes());
        data.push(0);
        data.resize(header + cmdsize, 0);

        let (is_macho, mutated, counts, _) = patch(&mut data, "/opt/glustore");
        assert!(is_macho);
        assert!(mutated);
        assert_eq!(counts.string_replacements, 1);
        // Re-read the string with the same big-endian layout.
        let name_off =
            u32::from_be_bytes(data[header + 8..header + 12].try_into().unwrap()) as usize;
        let start = header + name_off;
        let end = header + cmdsize;
        let nul = data[start..end].iter().position(|b| *b == 0).unwrap();
        assert_eq!(
            &data[start..start + nul],
            b"/opt/glustore/Cellar/ppc/1.0/lib/libppc.dylib"
        );
    }

    #[test]
    fn header_layout_parses_thin_macho() {
        let data = macho64(MH_DYLIB, &[dylib_cmd64(LC_ID_DYLIB, "libx.dylib")]);
        let layout = header_layout(&data).unwrap();
        assert_eq!(layout.header_size, 32);
        assert_eq!(layout.sizeofcmds, data.len() - 32);
        assert!(layout.little);
        assert!(header_layout(b"not a macho file").is_none());
        assert!(header_layout(&fat32(&[macho64(MH_DYLIB, &[])]).as_slice()[..8]).is_none());
    }

    #[test]
    fn signing_identifier_uses_lc_uuid_with_stem_prefix() {
        let uuid = [
            0xdeu8, 0xad, 0xbe, 0xef, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ];
        let mut uuid_cmd = vec![0u8; 24];
        uuid_cmd[0..4].copy_from_slice(&u32le(LC_UUID));
        uuid_cmd[4..8].copy_from_slice(&u32le(24));
        uuid_cmd[8..24].copy_from_slice(&uuid);
        let data = macho64(MH_DYLIB, &[uuid_cmd]);
        // ruby-macho: "<stem>-<hex>" where hex is hex("UUID") + uuid bytes.
        let id = signing_identifier(&data, "openssl", None).unwrap();
        assert_eq!(id, "openssl-55554944deadbeef0102030405060708090a0b0c");
    }

    #[test]
    fn signing_identifier_dot_stem_used_as_is() {
        let data = macho64(MH_DYLIB, &[]);
        assert_eq!(
            signing_identifier(&data, "libfoo.1", None).unwrap(),
            "libfoo.1"
        );
    }

    #[test]
    fn signing_identifier_prefers_info_plist_identifier() {
        let data = macho64(MH_DYLIB, &[]);
        let plist = b"<?xml version=\"1.0\"?>\n<plist>\n<key>CFBundleIdentifier</key>\n<string>com.example.Foo</string>\n</plist>";
        assert_eq!(
            signing_identifier(&data, "Foo", Some(plist)).unwrap(),
            "com.example.Foo"
        );
        // No plist: falls through to the stem/hex path.
        assert!(signing_identifier(&data, "Foo", None)
            .unwrap()
            .starts_with("Foo-"));
    }

    #[test]
    fn signing_identifier_falls_back_to_sha1() {
        // No LC_UUID: SHA-1 of the first 28 bytes plus the load-command region
        // (ruby-macho CodeSigning.identifier legacy path), prefixed with the stem.
        let data = macho64(MH_DYLIB, &[]);
        let id = signing_identifier(&data, "openssl", None).unwrap();
        assert_eq!(id.len(), "openssl-".len() + 40, "<stem>-<sha1 hex>");
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        ctx.update(&data[..28]);
        ctx.update(&data[32..]);
        let expected = crate::hash::hex_lower(ctx.finish().as_ref());
        assert_eq!(id, format!("openssl-{expected}"));
        assert!(signing_identifier(b"junk", "x", None).is_none());
    }

    #[test]
    fn section_range_finds_info_plist_in_segment() {
        // LC_SEGMENT_64 with one __TEXT/__info_plist section at file offset 0x4000.
        let mut seg = vec![0u8; 72 + 80];
        seg[0..4].copy_from_slice(&u32le(LC_SEGMENT_64));
        seg[4..8].copy_from_slice(&u32le((72 + 80) as u32));
        seg[8..8 + 16].copy_from_slice(b"__TEXT\0\0\0\0\0\0\0\0\0\0");
        seg[64..68].copy_from_slice(&u32le(1)); // nsects
                                                // section_64: sectname, segname, addr(8), size(8), offset(4), align(4), ...
        seg[72..72 + 16].copy_from_slice(b"__info_plist\0\0\0\0");
        seg[88..88 + 16].copy_from_slice(b"__TEXT\0\0\0\0\0\0\0\0\0\0");
        seg[72 + 32..72 + 36].copy_from_slice(&u32le(0x4000)); // offset
        seg[72 + 40..72 + 44].copy_from_slice(&u32le(64)); // size
        let data = macho64(MH_DYLIB, &[seg]);
        let layout = header_layout(&data).unwrap();
        assert_eq!(
            section_range(&data, layout, b"__TEXT", b"__info_plist"),
            Some((0x4000, 64))
        );
        assert_eq!(section_range(&data, layout, b"__TEXT", b"__text"), None);
    }
}
