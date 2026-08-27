use super::super::*;

pub(in crate::postinstall::structured) fn delete_keychain_certificate(
    ctx: &PostinstallContext<'_>,
    step: &Value,
) -> Result<()> {
    let mut expected: Option<String> = None;
    if let Some(spec) = step.get("matching_certificate") {
        let certificate = path(ctx, spec)?;
        if !certificate.exists() {
            return Ok(());
        }
        // openssl x509 -fingerprint -sha256 -noout: "sha256 Fingerprint=AB:CD:.."
        let out = run_command(RunCommand::new(
            Some(ctx.env),
            Path::new("/usr/bin/openssl"),
            &[
                "x509".into(),
                "-fingerprint".into(),
                "-sha256".into(),
                "-noout".into(),
                "-in".into(),
                certificate.display().to_string(),
            ],
        ))?;
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let hash = line
            .split('=')
            .nth(1)
            .unwrap_or("")
            .replace(':', "")
            .trim()
            .to_uppercase();
        if hash.is_empty() {
            return Ok(());
        }
        expected = Some(hash);
    }
    let name = step.get("name").and_then(Value::as_str).unwrap_or("");
    let out = run_command(
        RunCommand::new(
            Some(ctx.env),
            Path::new("/usr/bin/security"),
            &[
                "find-certificate".into(),
                "-a".into(),
                "-c".into(),
                name.to_string(),
                "-Z".into(),
            ],
        )
        .sudo(true),
    )?;
    // security find-certificate -Z lines: "SHA-256 hash: <40 hex chars>" (no colons)
    let hashes: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            line.strip_prefix("SHA-256 hash:")
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_uppercase())
        })
        .collect();
    if let Some(expected) = expected {
        if hashes.contains(&expected) {
            run_command(
                RunCommand::new(
                    Some(ctx.env),
                    Path::new("/usr/bin/security"),
                    &["delete-certificate".into(), "-Z".into(), expected],
                )
                .sudo(true),
            )?;
        }
    } else {
        for hash in hashes {
            run_command(
                RunCommand::new(
                    Some(ctx.env),
                    Path::new("/usr/bin/security"),
                    &["delete-certificate".into(), "-Z".into(), hash],
                )
                .sudo(true),
            )?;
        }
    }
    Ok(())
}
