# Identity and trust

- Outbound gossip messages are signed when a local signer is configured.
- Published skills are signed at the content level (signature embedded in the skill entry).
- Per-room policies can require signed messages and/or enforce whitelisted signer identities.
- Identity label format is:
  - `gpg:<key-id>`
  - `ssh:<public-key>`
- If a room has whitelist entries, messages from non-whitelisted identities are dropped.
- If `require_signed=true`, unsigned messages are dropped.
- Peer-scoped actions must come from the identity that joined under that display name. A different signer cannot impersonate that peer in file activity, status, or leave messages.
- Incoming skills with invalid hashes or embedded signatures are rejected, including search results. Signed-only room policies also require an embedded skill signature.
- Remote searches are always scoped to the receiving room. Memory/skill/task payloads claiming a different room are rejected.
- Signed messages carry a nonce and a `sent_at` timestamp: receivers drop signed messages older than 10 minutes (clock-skew tolerant) and messages whose nonce was already seen, so captured messages cannot be replayed.
- GPG identities should be whitelisted by full fingerprint — short key ids are collision-prone.

Example policy setup:

```json
{
  "tool": "set_identity_policy",
  "args": {
    "room": "feature-a",
    "identities": [
      "gpg:ABC123DEF456",
      "ssh:ssh-ed25519 AAAAC3Nza... alice@laptop"
    ],
    "require_signed": true
  }
}
```
