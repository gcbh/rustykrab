# Skills Issues

GitHub issues labeled **skills** (excluding those also labeled **documentation**).

## Critical

### #149 — SKL-C2: verify_skill_bundle concatenation ambiguity allows signature forgery
- **Labels:** critical, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/verify.rs:54-64`
- **Description:** `verify_skill_bundle` signs `manifest_bytes || code_bytes` without a length delimiter. Bytes can shift between boundaries while keeping concatenation identical, allowing signature forgery.
- **Suggested fix:** Prepend `manifest_bytes.len()` as an 8-byte little-endian integer.

## High

### #163 — SKL-H5: No symlink protection in skill loader
- **Labels:** high, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/loader.rs:24-35`
- **Description:** Directory iteration follows symbolic links. An attacker could create a symlink named `skills/evil` pointing to `/etc/` to read arbitrary files from the system.
- **Suggested fix:** Check `entry.file_type()?.is_symlink()` and skip symlink entries during directory traversal.

### #160 — SKL-H3: Persistent prompt injection via poisoned memories
- **Labels:** high, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/prompt.rs:112-119`
- **Description:** Marker strings like `[END RECALLED MEMORIES]` lack sanitization, allowing poisoned memory data to break out of the fence and inject arbitrary prompt content.
- **Suggested fix:** Strip or escape marker strings when processing summaries.

### #157 — SKL-H2: Prompt injection via unescaped skill body
- **Labels:** high, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/prompt.rs:205-211`
- **Description:** `with_active_skill` escapes the skill name but directly injects the skill body into XML without sanitization. An attacker can include `</skill_instructions>` in the body to break out of the XML structure and inject system-level instructions.
- **Suggested fix:** Implement CDATA wrapping for untrusted content within XML.

## Medium

### #180 — SKL-M6: Skill name not validated after parsing
- **Labels:** medium, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/skill_md.rs:71-98`
- **Description:** Skill name content is not validated after parsing, allowing problematic characters including slashes, null bytes, and unreasonably long strings.
- **Suggested fix:** Add validation for skill names after parsing.

### #173 — SKL-M4: with_available_skills leaks filesystem paths into prompts
- **Labels:** medium, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/prompt.rs:184-199`
- **Description:** Full filesystem paths are injected into the system prompt, leaking internal server directory structure.
- **Suggested fix:** Strip or replace filesystem paths before inserting into prompts.

### #164 — SKL-M1: Skill registry silent override on name collision
- **Labels:** medium, skills
- **State:** Open
- **File:** `crates/rustykrab-skills/src/skill.rs:48-58`
- **Description:** `register()` and `register_md()` use different keys for the same HashMap. Collisions silently overwrite existing entries.
- **Suggested fix:** Use consistent key generation and warn or error on name collisions.
