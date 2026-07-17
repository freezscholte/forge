use crate::{SignatureFinding, SignatureFindingKind, SignatureKeySummary};
use anyhow::{anyhow, Context, Result};
use forge_core::new_id;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const SIGNING_KEY_PATH: &str = ".forge/keys/local-ed25519.pk8";
const SIGNATURE_ALG: &str = "ed25519";
const TRUST_LEVEL: &str = "locally_signed";
const HOSTED_RUNNER_TRUST_LEVEL: &str = "hosted_runner_signed";
const THIRD_PARTY_TRUST_LEVEL: &str = "third_party_attested";

type SignedSubject = (String, String, String);
type ValidSignatureSet = BTreeSet<SignedSubject>;

struct ExpectedSignedSubjects {
    any_verifiable: Vec<SignedSubject>,
    local_only: Vec<SignedSubject>,
}

pub(crate) struct LocalSigner {
    key_pair: Ed25519KeyPair,
    public_key: String,
    key_fingerprint: String,
}

pub(crate) struct LocalKeyInfo {
    pub public_key: String,
    pub key_fingerprint: String,
    pub key_path: String,
    pub exists_before_command: bool,
}

pub(crate) struct ExternalAttestationSigner {
    key_pair: Ed25519KeyPair,
    public_key: String,
    key_fingerprint: String,
}

pub(crate) struct ExternalSignatureInput<'a> {
    pub repo_id: &'a str,
    pub subject_kind: &'a str,
    pub subject_id: &'a str,
    pub signed_digest: &'a str,
    pub trust_level: &'a str,
    pub trust_origin: &'a str,
    pub created_at_ms: i64,
}

pub(crate) struct RotatedLocalKey {
    pub previous_fingerprint: Option<String>,
    pub previous_key_backup_path: Option<String>,
    pub new_key: LocalKeyInfo,
}

impl LocalSigner {
    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|_| anyhow!("parse local Ed25519 signing key"))?;
        let public_key_bytes = key_pair.public_key().as_ref();
        let public_key = hex_lower(public_key_bytes);
        let key_fingerprint = key_fingerprint_for_public_key(public_key_bytes);
        Ok(Self {
            key_pair,
            public_key,
            key_fingerprint,
        })
    }

    pub(crate) fn load_existing(repo_root: &Path) -> Result<Option<Self>> {
        let path = repo_root.join(SIGNING_KEY_PATH);
        if !path.exists() {
            return Ok(None);
        }
        let pkcs8 = fs::read(&path).with_context(|| "read local signing key")?;
        Ok(Some(Self::from_pkcs8(&pkcs8)?))
    }

    pub(crate) fn load_or_create(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(SIGNING_KEY_PATH);
        let pkcs8 = if path.exists() {
            fs::read(&path).with_context(|| "read local signing key")?
        } else {
            let rng = SystemRandom::new();
            let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
                .map_err(|_| anyhow!("generate local Ed25519 signing key"))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| "create local signing key directory")?;
                set_private_dir_permissions(parent)?;
            }
            fs::write(&path, pkcs8.as_ref()).with_context(|| "write local signing key")?;
            set_private_file_permissions(&path)?;
            pkcs8.as_ref().to_vec()
        };
        Self::from_pkcs8(&pkcs8)
    }

    pub(crate) fn sign_subject(
        &self,
        tx: &Transaction<'_>,
        repo_id: &str,
        subject_kind: &str,
        subject_id: &str,
        signed_digest: &str,
        created_at_ms: i64,
    ) -> Result<()> {
        register_local_signing_key(
            tx,
            repo_id,
            &self.public_key,
            &self.key_fingerprint,
            created_at_ms,
        )?;
        let message = signing_message(subject_kind, subject_id, signed_digest);
        let signature = self.key_pair.sign(&message);
        tx.execute(
            "INSERT OR IGNORE INTO ledger_signatures (
                id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
                public_key, key_fingerprint, signature, trust_level, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new_id("sig"),
                repo_id,
                subject_kind,
                subject_id,
                signed_digest,
                SIGNATURE_ALG,
                self.public_key,
                self.key_fingerprint,
                hex_lower(signature.as_ref()),
                TRUST_LEVEL,
                created_at_ms
            ],
        )?;
        Ok(())
    }
}

impl ExternalAttestationSigner {
    pub(crate) fn load_from_pkcs8(path: &Path) -> Result<Self> {
        let pkcs8 = fs::read(path).with_context(|| "read external attestation signing key")?;
        let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
            .or_else(|_| Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8))
            .map_err(|_| anyhow!("parse external attestation Ed25519 signing key"))?;
        let public_key_bytes = key_pair.public_key().as_ref().to_vec();
        Ok(Self {
            key_pair,
            public_key: hex_lower(&public_key_bytes),
            key_fingerprint: key_fingerprint_for_public_key(&public_key_bytes),
        })
    }

    pub(crate) fn key_fingerprint(&self) -> &str {
        &self.key_fingerprint
    }

    pub(crate) fn public_key(&self) -> &str {
        &self.public_key
    }

    pub(crate) fn sign_subject(
        &self,
        tx: &Transaction<'_>,
        input: ExternalSignatureInput<'_>,
    ) -> Result<bool> {
        register_external_signing_key(
            tx,
            input.repo_id,
            &self.public_key,
            &self.key_fingerprint,
            input.trust_origin,
            input.created_at_ms,
        )?;
        let message = signing_message_for_trust(
            input.trust_level,
            input.subject_kind,
            input.subject_id,
            input.signed_digest,
        );
        let signature = self.key_pair.sign(&message);
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO ledger_signatures (
                id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
                public_key, key_fingerprint, signature, trust_level, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new_id("sig"),
                input.repo_id,
                input.subject_kind,
                input.subject_id,
                input.signed_digest,
                SIGNATURE_ALG,
                self.public_key,
                self.key_fingerprint,
                hex_lower(signature.as_ref()),
                input.trust_level,
                input.created_at_ms
            ],
        )?;
        Ok(inserted > 0)
    }
}

pub(crate) fn local_key_status(repo_root: &Path) -> Result<LocalKeyInfo> {
    let key_path = repo_root.join(SIGNING_KEY_PATH);
    let exists_before_command = key_path.exists();
    let signer = LocalSigner::load_or_create(repo_root)?;
    Ok(LocalKeyInfo {
        public_key: signer.public_key,
        key_fingerprint: signer.key_fingerprint,
        key_path: SIGNING_KEY_PATH.to_string(),
        exists_before_command,
    })
}

pub(crate) fn existing_local_key_info(repo_root: &Path) -> Result<Option<LocalKeyInfo>> {
    Ok(
        LocalSigner::load_existing(repo_root)?.map(|signer| LocalKeyInfo {
            public_key: signer.public_key,
            key_fingerprint: signer.key_fingerprint,
            key_path: SIGNING_KEY_PATH.to_string(),
            exists_before_command: true,
        }),
    )
}

pub(crate) fn rotate_local_key(repo_root: &Path) -> Result<RotatedLocalKey> {
    let key_path = repo_root.join(SIGNING_KEY_PATH);
    let previous = if key_path.exists() {
        let signer = LocalSigner::load_or_create(repo_root)?;
        let backup = rotated_key_path(repo_root, &signer.key_fingerprint);
        if !backup.exists() {
            fs::copy(&key_path, &backup).with_context(|| "backup previous local signing key")?;
            set_private_file_permissions(&backup)?;
        }
        Some((signer.key_fingerprint, backup))
    } else {
        None
    };

    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow!("generate rotated local Ed25519 signing key"))?;
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent).with_context(|| "create local signing key directory")?;
        set_private_dir_permissions(parent)?;
    }
    fs::write(&key_path, pkcs8.as_ref()).with_context(|| "write rotated local signing key")?;
    set_private_file_permissions(&key_path)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| anyhow!("parse rotated local Ed25519 signing key"))?;
    let public_key_bytes = key_pair.public_key().as_ref();
    let new_key = LocalKeyInfo {
        public_key: hex_lower(public_key_bytes),
        key_fingerprint: key_fingerprint_for_public_key(public_key_bytes),
        key_path: SIGNING_KEY_PATH.to_string(),
        exists_before_command: true,
    };

    Ok(RotatedLocalKey {
        previous_fingerprint: previous
            .as_ref()
            .map(|(fingerprint, _)| fingerprint.clone()),
        previous_key_backup_path: previous
            .as_ref()
            .map(|(_, path)| relative_key_path(path).unwrap_or_else(|| path.display().to_string())),
        new_key,
    })
}

pub(crate) fn verify_signatures(conn: &Connection) -> Result<Vec<SignatureFinding>> {
    let state = verified_signature_state(conn)?;
    let mut findings = state.findings;
    let expected = expected_signed_subjects(conn)?;

    findings.extend(missing_signature_findings(
        &state.valid_any,
        expected.any_verifiable,
    ));
    findings.extend(missing_signature_findings(
        &state.valid_local,
        expected.local_only,
    ));

    Ok(findings)
}

fn rotated_key_path(repo_root: &Path, fingerprint: &str) -> std::path::PathBuf {
    repo_root
        .join(".forge/keys")
        .join(format!("local-ed25519-{fingerprint}.pk8"))
}

fn relative_key_path(path: &Path) -> Option<String> {
    let keys = path.parent()?;
    let forge = keys.parent()?;
    if forge.file_name()?.to_str()? != ".forge" {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    Some(format!(".forge/keys/{name}"))
}

pub(crate) fn verify_subject_signatures(
    conn: &Connection,
    subjects: Vec<SignedSubject>,
) -> Result<Vec<SignatureFinding>> {
    verify_subject_signatures_with_scope(conn, subjects, SignatureTrustScope::AnyVerifiable)
}

pub(crate) fn verify_subject_local_signatures(
    conn: &Connection,
    subjects: Vec<SignedSubject>,
) -> Result<Vec<SignatureFinding>> {
    verify_subject_signatures_with_scope(conn, subjects, SignatureTrustScope::LocalOnly)
}

pub(crate) fn verify_subject_hosted_runner_signatures(
    conn: &Connection,
    subjects: Vec<SignedSubject>,
) -> Result<Vec<SignatureFinding>> {
    verify_subject_signatures_with_scope(conn, subjects, SignatureTrustScope::HostedRunner)
}

pub(crate) fn verify_subject_third_party_signatures(
    conn: &Connection,
    subjects: Vec<SignedSubject>,
) -> Result<Vec<SignatureFinding>> {
    verify_subject_signatures_with_scope(conn, subjects, SignatureTrustScope::ThirdParty)
}

fn verify_subject_signatures_with_scope(
    conn: &Connection,
    subjects: Vec<SignedSubject>,
    scope: SignatureTrustScope,
) -> Result<Vec<SignatureFinding>> {
    let state = verified_signature_state(conn)?;
    let required: BTreeSet<SignedSubject> = subjects.iter().cloned().collect();
    let mut scoped: Vec<SignatureFinding> = state
        .findings
        .into_iter()
        .filter(|finding| {
            required
                .iter()
                .any(|(kind, id, _)| kind == &finding.subject_kind && id == &finding.subject_id)
        })
        .collect();
    let valid = match scope {
        SignatureTrustScope::AnyVerifiable => &state.valid_any,
        SignatureTrustScope::LocalOnly => &state.valid_local,
        SignatureTrustScope::HostedRunner => &state.valid_hosted_runner,
        SignatureTrustScope::ThirdParty => &state.valid_third_party,
    };
    scoped.extend(missing_signature_findings(valid, subjects));
    Ok(scoped)
}

pub(crate) fn verified_subject_fingerprint(
    conn: &Connection,
    subject_kind: &str,
    subject_id: &str,
    signed_digest: &str,
) -> Result<(Option<String>, Vec<SignatureFinding>)> {
    let subject = (
        subject_kind.to_string(),
        subject_id.to_string(),
        signed_digest.to_string(),
    );
    let issues = verify_subject_signatures(conn, vec![subject])?;
    if issues.is_empty() {
        let fingerprint = conn
            .query_row(
                "SELECT key_fingerprint
                 FROM ledger_signatures
                 WHERE subject_kind = ?1 AND subject_id = ?2 AND signed_digest = ?3
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![subject_kind, subject_id, signed_digest],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        return Ok((fingerprint, Vec::new()));
    }

    if issues.iter().all(|issue| {
        issue.kind == SignatureFindingKind::MissingSignature
            && issue.subject_kind == subject_kind
            && issue.subject_id == subject_id
    }) && legacy_subject(conn, subject_kind, subject_id)?
    {
        return Ok((None, Vec::new()));
    }

    Ok((None, issues))
}

#[derive(Debug, Clone, Copy)]
enum SignatureTrustScope {
    AnyVerifiable,
    LocalOnly,
    HostedRunner,
    ThirdParty,
}

struct VerifiedSignatureState {
    valid_any: ValidSignatureSet,
    valid_local: ValidSignatureSet,
    valid_hosted_runner: ValidSignatureSet,
    valid_third_party: ValidSignatureSet,
    findings: Vec<SignatureFinding>,
}

fn verified_signature_state(conn: &Connection) -> Result<VerifiedSignatureState> {
    let mut findings = Vec::new();
    let mut valid_any = BTreeSet::new();
    let mut valid_local = BTreeSet::new();
    let mut valid_hosted_runner = BTreeSet::new();
    let mut valid_third_party = BTreeSet::new();

    let mut stmt = conn.prepare(
        "SELECT
            ls.subject_kind,
            ls.subject_id,
            ls.signed_digest,
            ls.public_key,
            ls.key_fingerprint,
            ls.signature,
            ls.trust_level,
            COALESCE(sk.trust_origin, 'peer')
         FROM ledger_signatures ls
         LEFT JOIN signing_keys sk
            ON sk.repo_id = ls.repo_id
           AND sk.key_fingerprint = ls.key_fingerprint
           AND sk.public_key = ls.public_key
         ORDER BY ls.rowid",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;

    for row in rows {
        let (
            subject_kind,
            subject_id,
            signed_digest,
            public_key,
            key_fingerprint,
            signature,
            trust_level,
            trust_origin,
        ) = row?;
        match current_subject_digest(conn, &subject_kind, &subject_id)? {
            None => findings.push(finding(
                SignatureFindingKind::SubjectMissing,
                &subject_kind,
                &subject_id,
                Some(&key_fingerprint),
            )),
            Some(current) if current != signed_digest => findings.push(finding(
                SignatureFindingKind::DigestMismatch,
                &subject_kind,
                &subject_id,
                Some(&key_fingerprint),
            )),
            Some(_) => {
                let public_key_bytes = match hex_decode(&public_key) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        findings.push(finding(
                            SignatureFindingKind::MalformedSignature,
                            &subject_kind,
                            &subject_id,
                            Some(&key_fingerprint),
                        ));
                        continue;
                    }
                };
                let signature_bytes = match hex_decode(&signature) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        findings.push(finding(
                            SignatureFindingKind::MalformedSignature,
                            &subject_kind,
                            &subject_id,
                            Some(&key_fingerprint),
                        ));
                        continue;
                    }
                };
                let message = signing_message_for_trust(
                    &trust_level,
                    &subject_kind,
                    &subject_id,
                    &signed_digest,
                );
                let recomputed_fingerprint = key_fingerprint_for_public_key(&public_key_bytes);
                if recomputed_fingerprint != key_fingerprint {
                    findings.push(finding(
                        SignatureFindingKind::MalformedSignature,
                        &subject_kind,
                        &subject_id,
                        Some(&key_fingerprint),
                    ));
                    continue;
                }
                if UnparsedPublicKey::new(&ED25519, public_key_bytes)
                    .verify(&message, &signature_bytes)
                    .is_ok()
                {
                    let signed_subject = (subject_kind, subject_id, signed_digest);
                    if trust_level == TRUST_LEVEL && trust_origin == "local" {
                        valid_local.insert(signed_subject.clone());
                    }
                    if trust_level == HOSTED_RUNNER_TRUST_LEVEL && trust_origin == "hosted_runner" {
                        valid_hosted_runner.insert(signed_subject.clone());
                    }
                    if trust_level == THIRD_PARTY_TRUST_LEVEL && trust_origin == "third_party" {
                        valid_third_party.insert(signed_subject.clone());
                    }
                    valid_any.insert(signed_subject);
                } else {
                    findings.push(finding(
                        SignatureFindingKind::InvalidSignature,
                        &subject_kind,
                        &subject_id,
                        Some(&key_fingerprint),
                    ));
                }
            }
        }
    }

    Ok(VerifiedSignatureState {
        valid_any,
        valid_local,
        valid_hosted_runner,
        valid_third_party,
        findings,
    })
}

pub(crate) fn register_local_signing_key(
    conn: &Connection,
    repo_id: &str,
    public_key: &str,
    key_fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO signing_keys (
            repo_id, key_fingerprint, public_key, trust_origin, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, 'local', ?4, ?4)
         ON CONFLICT(repo_id, key_fingerprint) DO UPDATE SET
            public_key = excluded.public_key,
            trust_origin = 'local',
            updated_at_ms = excluded.updated_at_ms",
        params![repo_id, key_fingerprint, public_key, now_ms],
    )?;
    Ok(())
}

pub(crate) fn register_external_signing_key(
    conn: &Connection,
    repo_id: &str,
    public_key: &str,
    key_fingerprint: &str,
    trust_origin: &str,
    now_ms: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO signing_keys (
            repo_id, key_fingerprint, public_key, trust_origin, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(repo_id, key_fingerprint) DO UPDATE SET
            public_key = excluded.public_key,
            trust_origin = excluded.trust_origin,
            updated_at_ms = excluded.updated_at_ms",
        params![repo_id, key_fingerprint, public_key, trust_origin, now_ms],
    )?;
    Ok(())
}

pub(crate) fn signature_key_summary(
    conn: &Connection,
    repo_id: &str,
) -> Result<SignatureKeySummary> {
    let mut summary = SignatureKeySummary::default();
    let mut stmt = conn.prepare(
        "SELECT key_fingerprint, trust_origin
         FROM signing_keys
         WHERE repo_id = ?1
         ORDER BY trust_origin, key_fingerprint",
    )?;
    let rows = stmt.query_map(params![repo_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (fingerprint, origin) = row?;
        match origin.as_str() {
            "local" => summary.local_key_fingerprints.push(fingerprint),
            "peer" => summary.peer_key_fingerprints.push(fingerprint),
            "hosted_runner" => summary.hosted_runner_key_fingerprints.push(fingerprint),
            "third_party" => summary.third_party_key_fingerprints.push(fingerprint),
            _ => {}
        }
    }
    Ok(summary)
}

fn legacy_subject(conn: &Connection, subject_kind: &str, subject_id: &str) -> Result<bool> {
    let marker = signature_marker(conn)?;
    match subject_kind {
        "evidence" => {
            let rowid = conn
                .query_row(
                    "SELECT rowid FROM evidence WHERE id = ?1",
                    params![subject_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            Ok(rowid.is_some_and(|rowid| rowid <= marker.evidence_high_water))
        }
        "decision" => {
            let rowid = conn
                .query_row(
                    "SELECT rowid FROM decisions WHERE id = ?1",
                    params![subject_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            Ok(rowid.is_some_and(|rowid| rowid <= marker.decision_high_water))
        }
        _ => Ok(false),
    }
}

fn missing_signature_findings(
    valid: &ValidSignatureSet,
    subjects: Vec<SignedSubject>,
) -> Vec<SignatureFinding> {
    let mut findings = Vec::new();
    for (subject_kind, subject_id, signed_digest) in subjects {
        if !valid.contains(&(
            subject_kind.clone(),
            subject_id.clone(),
            signed_digest.clone(),
        )) {
            findings.push(finding(
                SignatureFindingKind::MissingSignature,
                &subject_kind,
                &subject_id,
                None,
            ));
        }
    }
    findings
}

fn expected_signed_subjects(conn: &Connection) -> Result<ExpectedSignedSubjects> {
    let marker = signature_marker(conn)?;
    let mut any_verifiable = Vec::new();
    let local_only = Vec::new();

    let mut evidence = conn.prepare(
        "SELECT id, content_hash FROM evidence
         WHERE rowid > ?1 AND content_hash IS NOT NULL
         ORDER BY rowid",
    )?;
    for row in evidence.query_map(params![marker.evidence_high_water], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, digest) = row?;
        any_verifiable.push(("evidence".to_string(), id, digest));
    }

    let mut decisions = conn.prepare(
        "SELECT id, content_hash, commit_id FROM decisions
         WHERE rowid > ?1 AND content_hash IS NOT NULL
         ORDER BY rowid",
    )?;
    for row in decisions.query_map(params![marker.decision_high_water], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (id, digest, commit_id) = row?;
        any_verifiable.push(("decision".to_string(), id, digest));
        if let Some(commit_id) = commit_id {
            any_verifiable.push(("commit".to_string(), commit_id.clone(), commit_id));
        }
    }

    let query = format!(
        "SELECT json_extract(v.state_json, '$.commit_id')
           FROM operations o
           JOIN views v ON v.id = o.resulting_view_id
          WHERE o.kind IN ({})
            AND json_extract(v.state_json, '$.commit_id') IS NOT NULL
          ORDER BY o.rowid",
        crate::sync::SYNC_MERGED_OP_KIND_SQL_IN
    );
    let mut sync_merges = conn.prepare(&query)?;
    for row in sync_merges.query_map([], |row| row.get::<_, String>(0))? {
        let commit_id = row?;
        any_verifiable.push((
            "sync_merge_commit".to_string(),
            commit_id.clone(),
            commit_id,
        ));
    }

    // Contract-family rows (U9/KTD2): every contract, run, stop, and verdict row must
    // carry a valid local signature over its current digest. The four kinds are born
    // signed in migration 022, so there is no pre-signing population to grandfather
    // (per-kind high-water 0); the enumeration lives in the contract domain module.
    any_verifiable.extend(crate::contract::expected_contract_signed_subjects(conn)?);

    Ok(ExpectedSignedSubjects {
        any_verifiable,
        local_only,
    })
}

fn current_subject_digest(
    conn: &Connection,
    subject_kind: &str,
    subject_id: &str,
) -> Result<Option<String>> {
    match subject_kind {
        "evidence" => conn
            .query_row(
                "SELECT content_hash FROM evidence WHERE id = ?1",
                params![subject_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(Into::into),
        "decision" => conn
            .query_row(
                "SELECT content_hash FROM decisions WHERE id = ?1",
                params![subject_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|value| value.flatten())
            .map_err(Into::into),
        "commit" => {
            let exists = conn
                .query_row(
                    "SELECT 1 FROM decisions WHERE commit_id = ?1 LIMIT 1",
                    params![subject_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            Ok(exists.then(|| subject_id.to_string()))
        }
        "sync_merge_commit" => {
            let query = format!(
                "SELECT 1
                       FROM operations o
                       JOIN views v ON v.id = o.resulting_view_id
                      WHERE o.kind IN ({})
                        AND json_extract(v.state_json, '$.commit_id') = ?1
                     LIMIT 1",
                crate::sync::SYNC_MERGED_OP_KIND_SQL_IN
            );
            let exists = conn
                .query_row(&query, params![subject_id], |_| Ok(()))
                .optional()?
                .is_some();
            Ok(exists.then(|| subject_id.to_string()))
        }
        // Contract-family kinds (U9/KTD2): recompute the subject's canonical digest
        // from its CURRENT row content so an out-of-band field edit is caught as a
        // `DigestMismatch` and a deleted row as a `SubjectMissing`. The recompute
        // lives in the contract domain module (it owns the digest functions).
        "contract" | "contract_run" | "contract_stop" | "contract_run_verdict" => {
            crate::contract::contract_subject_digest(conn, subject_kind, subject_id)
        }
        _ => Ok(None),
    }
}

struct SignatureMarker {
    evidence_high_water: i64,
    decision_high_water: i64,
}

fn signature_marker(conn: &Connection) -> Result<SignatureMarker> {
    conn.query_row(
        "SELECT evidence_high_water, decision_high_water
         FROM signature_marker WHERE singleton = 1",
        [],
        |row| {
            Ok(SignatureMarker {
                evidence_high_water: row.get(0)?,
                decision_high_water: row.get(1)?,
            })
        },
    )
    .map_err(Into::into)
}

fn signing_message(subject_kind: &str, subject_id: &str, signed_digest: &str) -> Vec<u8> {
    format!(
        "forge-ledger-signature-v1\nsubject_kind={subject_kind}\nsubject_id={subject_id}\nsigned_digest={signed_digest}\n"
    )
    .into_bytes()
}

fn signing_message_for_trust(
    trust_level: &str,
    subject_kind: &str,
    subject_id: &str,
    signed_digest: &str,
) -> Vec<u8> {
    if trust_level == HOSTED_RUNNER_TRUST_LEVEL || trust_level == THIRD_PARTY_TRUST_LEVEL {
        return format!(
            "forge-ledger-signature-v2\ntrust_level={trust_level}\nsubject_kind={subject_kind}\nsubject_id={subject_id}\nsigned_digest={signed_digest}\n"
        )
        .into_bytes();
    }
    signing_message(subject_kind, subject_id, signed_digest)
}

fn finding(
    kind: SignatureFindingKind,
    subject_kind: &str,
    subject_id: &str,
    key_fingerprint: Option<&str>,
) -> SignatureFinding {
    SignatureFinding {
        kind,
        subject_kind: subject_kind.to_string(),
        subject_id: subject_id.to_string(),
        key_fingerprint: key_fingerprint.map(ToString::to_string),
    }
}

pub(crate) fn key_fingerprint_for_public_key(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"forge-ed25519-public-key-v1\n");
    hasher.update(public_key);
    hex_lower(&hasher.finalize()[..16])
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(anyhow!("odd-length hex"));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(anyhow!("invalid hex")),
    }
}

#[cfg(unix)]
pub(crate) fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}
