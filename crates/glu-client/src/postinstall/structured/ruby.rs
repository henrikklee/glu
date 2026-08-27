use super::*;

// Utils::Inreplace.inreplace with a regexp (install_steps.rb:1026-1036 +
// utils/inreplace.rb): multi-line regexp replace-all with audit, atomic write,
// and Ruby backref translation. Used by the configure_php / bootstrap_cpython
// actions; regexp EXTENDED caveats are shared with `inreplace` below.
pub(super) fn inreplace_regexp_file(path: &Path, regexp: &str, replacement: &str) -> Result<()> {
    let data = fs::read(path)?;
    let mut builder = regex::bytes::RegexBuilder::new(regexp);
    builder.multi_line(true);
    let regex = builder.build()?;
    let replacements = regex.find_iter(&data).count();
    if replacements == 0 {
        bail!("postinstall inreplace made no changes: {}", path.display());
    }
    let replacement = translate_backrefs_regexp(replacement);
    let replaced = regex.replace_all(&data, replacement.as_bytes());
    atomic_write(path, replaced.as_ref())?;
    Ok(())
}

// Homebrew 4dacfe77: install_steps.rb:1026-1036 (inreplace case) + utils/inreplace.rb
// + utils/string_inreplace_extension.rb:26-45. Port notes:
// - File.binread → byte reads (`regex::bytes`), so binary files match byte-wise
//   exactly like Ruby (verified: "\xFFab".gsub(/a/, "X") → "\xFFXb").
// - Ruby gsub/sub replacement escapes (`\1`, `\&`, `\0`, `\k<name>`, `\\`,
//   literal `$`) are translated to Rust regex `$` syntax via
//   `translate_backrefs_regexp` (verified against ruby 2.6: `\1`→group,
//   `$1`→literal, `\\1`→literal `\1`, unknown `\x`→literal).
// - STRING patterns have no capture groups: `\1`-`\9`/`\k<name>` expand to
//   empty, `\&`/`\0` to the matched literal, `\\` to `\` (verified:
//   "abc".gsub("b", "\\1") → "ac").
// - Regexp::EXTENDED (bit 2) is normalized by `strip_extended` (drop
//   unescaped whitespace + `#` comments outside classes while preserving class
//   bodies, POSIX class markers, escapes, and leading `]` literals).
// - atomic_write restores the original mode (ownership best-effort).
// - Audit message mirrors upstream's "expected replacement of X with Y".
// Provenance note: upstream can aggregate per-path errors in
// Utils::Inreplace::Error; structured `inreplace` steps carry a single path, so
// glu reports that path's error directly.
pub(super) fn inreplace(ctx: &PostinstallContext<'_>, step: &Value) -> Result<()> {
    let p = path(ctx, req(step, "path")?)?;
    let data = fs::read(&p)?; // File.binread
    let before = expand(
        ctx,
        step.get("before").and_then(Value::as_str).unwrap_or(""),
    );
    let after = expand(ctx, step.get("after").and_then(Value::as_str).unwrap_or(""));
    let first_only = bool_or(step, "first_only", false);
    let (new, replacements) = if bool_or(step, "regexp", false) {
        let options = step
            .get("regexp_options")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let pattern = if options & 2 != 0 {
            strip_extended(&before) // Regexp::EXTENDED (free-spacing)
        } else {
            before.clone()
        };
        let mut builder = regex::bytes::RegexBuilder::new(&pattern);
        builder.multi_line(true); // Ruby default ^/$ line anchors
        if options & 1 != 0 {
            builder.case_insensitive(true); // Regexp::IGNORECASE
        }
        if options & 4 != 0 {
            builder.dot_matches_new_line(true); // Regexp::MULTILINE
        }
        let regex = builder.build()?;
        let after = translate_backrefs_regexp(&after);
        if first_only {
            let replacements = usize::from(regex.is_match(&data));
            (
                regex.replacen(&data, 1, after.as_bytes()).to_vec(),
                replacements,
            )
        } else {
            let replacements = regex.find_iter(&data).count();
            (
                regex.replace_all(&data, after.as_bytes()).to_vec(),
                replacements,
            )
        }
    } else if first_only {
        let replacements = usize::from(find_subslice(&data, before.as_bytes()).is_some());
        (replace_literal(&data, &before, &after, false), replacements)
    } else {
        let replacements = count_subslice(&data, before.as_bytes());
        (replace_literal(&data, &before, &after, true), replacements)
    };
    if replacements == 0 && !bool_or(step, "skip_audit", false) {
        bail!(
            "inreplace failed\n{}:\n  expected replacement of {before:?} with {after:?}",
            p.display()
        );
    }
    atomic_write(&p, &new)?;
    Ok(())
}

// Homebrew 4dacfe77: extend/file/atomic.rb — write to a random Tempfile in
// the same directory + rename, then restore original ownership/mode best-effort
// (EPERM/EACCES ignored). Random same-dir temps avoid predictable-name symlink
// races while preserving the atomic same-filesystem rename behavior.
pub(super) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let old_meta = fs::metadata(path).ok();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let prefix = format!(
        ".{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)
        .with_context(|| format!("creating atomic temp beside {}", path.display()))?;
    tmp.write_all(data)
        .with_context(|| format!("writing atomic temp for {}", path.display()))?;
    tmp.flush()
        .with_context(|| format!("flushing atomic temp for {}", path.display()))?;

    if let Some(meta) = &old_meta {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Err(err) = chown_path(tmp.path(), meta.uid(), meta.gid(), false) {
                if !matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
                    return Err(err.into());
                }
            }
        }
        let _ = chmod_mode(tmp.path(), meta.permissions().mode());
    }

    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("renaming atomic temp to {}", path.display()))?;
    Ok(())
}

// Ruby gsub/sub replacement escapes → Rust regex replacement syntax. Ruby:
// `\\`→`\`, `\&`/`\0`→whole match, `\1`-`\9`→capture, `\k<name>`→named,
// unknown `\x`→literal, and `$` is LITERAL in Ruby (verified). Rust: `$0` whole
// match, `$1` capture, `$name` named, `$$` literal `$`.
fn translate_backrefs_regexp(after: &str) -> String {
    let mut chars = after.chars().peekable();
    let mut out = String::with_capacity(after.len() + 8);
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\\') => out.push('\\'),
                Some('&') | Some('0') => out.push_str("$0"),
                Some(d @ '1'..='9') => {
                    // `${N}` not `$N`: `$1new` would parse as named group "1new".
                    out.push_str("${");
                    out.push(d);
                    out.push('}');
                }
                Some('k') => {
                    if chars.peek() == Some(&'<') {
                        chars.next();
                        let mut name = String::new();
                        while let Some(&ch) = chars.peek() {
                            if ch == '>' {
                                chars.next();
                                break;
                            }
                            name.push(ch);
                            chars.next();
                        }
                        out.push('$');
                        out.push_str(&name);
                    } else {
                        out.push_str("\\k");
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out
}

// Ruby Regexp::EXTENDED (free-spacing): drop unescaped whitespace and `#`
// comments-to-EOL outside character classes. Character class bodies are copied
// byte-for-byte except for normal regex escapes; leading `]` (and `[^]`) stays
// literal, and POSIX class markers such as `[[:alpha:]]` do not prematurely end
// the outer class.
pub(super) fn strip_extended(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    let mut in_class = false;
    let mut class_start = false;
    while let Some((_, c)) = chars.next() {
        if c == '\\' {
            out.push('\\');
            if let Some((_, n)) = chars.next() {
                out.push(n);
            }
            if in_class && c != '^' {
                class_start = false;
            }
            continue;
        }

        if in_class {
            if c == '['
                && matches!(
                    chars.peek().map(|(_, ch)| *ch),
                    Some(':') | Some('.') | Some('=')
                )
            {
                out.push('[');
                let marker = chars.next().map(|(_, ch)| ch).expect("peeked marker");
                out.push(marker);
                copy_posix_class_marker(marker, &mut chars, &mut out);
                class_start = false;
                continue;
            }
            if c == ']' && !class_start {
                in_class = false;
                out.push(']');
                continue;
            }
            out.push(c);
            if !(class_start && c == '^') {
                class_start = false;
            }
            continue;
        }

        match c {
            '[' => {
                in_class = true;
                class_start = true;
                out.push('[');
            }
            '#' => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            other => out.push(other),
        }
    }
    out
}

fn copy_posix_class_marker(
    marker: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    out: &mut String,
) {
    while let Some((_, ch)) = chars.next() {
        out.push(ch);
        if ch == marker && matches!(chars.peek().map(|(_, next)| *next), Some(']')) {
            let (_, close) = chars.next().expect("peeked closing bracket");
            out.push(close);
            break;
        }
    }
}

// Ruby String#sub!/gsub! with a STRING pattern: literal byte match; the
// replacement expands `\\`→`\`, `\&`/`\0`→whole match, `\1`-`\9`/`\k<name>`→""
// (string patterns have no capture groups), unknown `\x`→literal, `$`→literal
// (verified against ruby 2.6).
fn replace_literal(data: &[u8], before: &str, after: &str, all: bool) -> Vec<u8> {
    let needle = before.as_bytes();
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while let Some(pos) = find_subslice(&data[i..], needle) {
        let m = i + pos;
        out.extend_from_slice(&data[i..m]);
        expand_literal_replacement(after, &data[m..m + needle.len()], &mut out);
        i = m + needle.len();
        if !all {
            break;
        }
    }
    out.extend_from_slice(&data[i..]);
    out
}

fn expand_literal_replacement(after: &str, matched: &[u8], out: &mut Vec<u8>) {
    let mut chars = after.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\\') => out.push(b'\\'),
                Some('&') | Some('0') => out.extend_from_slice(matched),
                // \1-\9 / \k<name> → empty: string patterns have no capture groups
                Some('1'..='9') => {}
                Some('k') => {
                    if chars.peek() == Some(&'<') {
                        chars.next();
                        while let Some(&ch) = chars.peek() {
                            chars.next();
                            if ch == '>' {
                                break;
                            }
                        }
                    }
                }
                Some(other) => {
                    let mut buf = [0u8; 4];
                    out.push(b'\\');
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => out.push(b'\\'),
            },
            other => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    memchr::memmem::find(haystack, needle)
}

fn count_subslice(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return haystack.len() + 1;
    }
    let mut n = 0;
    let mut rest = haystack;
    while let Some(pos) = memchr::memmem::find(rest, needle) {
        n += 1;
        rest = &rest[pos + needle.len()..];
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_extended_preserves_ruby_character_class_shape() {
        assert_eq!(strip_extended(" a # comment\n b "), "ab");
        assert_eq!(strip_extended(r"a\ b"), r"a\ b");
        assert_eq!(strip_extended("[ ] # comment\n x"), "[ ]x");
        assert_eq!(strip_extended("[]a] [^]b]"), "[]a][^]b]");
        assert_eq!(
            strip_extended("[[:alpha:] #] # comment\n x"),
            "[[:alpha:] #]x"
        );
        let escaped_space = regex::bytes::Regex::new(&strip_extended(r"a\ b")).unwrap();
        assert!(escaped_space.is_match(b"a b"));
    }
}
