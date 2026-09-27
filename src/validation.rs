//! Validation shared by skill publication and distributed search ingress.
use crate::identity::verify_signature;
use crate::skill::SkillEntry;

pub async fn validate_skill(
    room: &str,
    entry: &SkillEntry,
    whitelist: &[String],
    require_signed: bool,
) -> bool {
    if entry.room != room || !entry.verify_content_hash() {
        return false;
    }
    let Some(identity) = &entry.signed_by else {
        return entry.signature.is_none() && !require_signed && whitelist.is_empty();
    };
    if !whitelist.is_empty() && !whitelist.contains(&identity.to_label()) {
        return false;
    }
    let Some(signature) = &entry.signature else {
        return false;
    };
    match verify_signature(identity, &entry.signing_payload(), signature).await {
        Ok(valid) => valid,
        Err(error) => {
            tracing::warn!(%error, "skill signature verification failed");
            false
        }
    }
}
