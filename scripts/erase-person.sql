-- Erase one person from the content hub (UK GDPR Art. 17; docs/PERSONAL_DATA.md).
--
--   psql "<owner DATABASE_URL>" -v subject='<the identity provider subject>' -f scripts/erase-person.sql
--
-- Run it as the schema OWNER (the `migrate` role; the application role cannot touch the audit log, by design) or as a
-- superuser. `subject` is the Cloudflare Access `sub` claim (a UUID) shown in `hub.users`. What it does, in ONE transaction:
--   1. picks a random pseudonym ("erased-<uuid>") for the person;
--   2. replaces the subject by the pseudonym in every attribution column (posts, post revisions, spend caps, settings,
--      the audit log's actor and its before/after documents), so history keeps its shape but names nobody;
--   3. deletes the person's `hub.users` row (display name, first/last seen); task assignments become unassigned;
--   4. checks that the subject appears nowhere any more and aborts the whole transaction otherwise.
-- The audit log is append-only for everyone (a trigger); step 2 lifts that trigger for this transaction only and restores it.
-- `updated_at` of the touched posts changes (their trigger); nothing else about their content does.
-- It does NOT touch: the signed Walrus snapshots (they hold public roadmap text only, never people), backups (they age out:
-- 14 offsite sets, 30-day bucket rule, 14 in-cluster), and Cloudflare's own Access logs (the identity provider's, not the hub's).
\set ON_ERROR_STOP on
\if :{?subject}
\else
  -- (psql cannot set an exit status from \quit, so a deliberate type error makes it stop with a non-zero status and this message)
  SELECT 'usage: psql <owner url> -v subject=<subject> -f scripts/erase-person.sql'::int;
\endif

BEGIN;

SELECT 'erased-' || gen_random_uuid() AS pseudo \gset

-- Refuse to run for a subject the hub has never stored (a typo would otherwise "succeed").
SELECT EXISTS (SELECT 1 FROM hub.users WHERE subject = :'subject')
    OR EXISTS (SELECT 1 FROM hub.audit_log WHERE actor_subject = :'subject') AS known \gset
\if :known
\else
  ROLLBACK;
  SELECT 'no stored data for that subject; nothing changed'::int;
\endif

ALTER TABLE hub.audit_log DISABLE TRIGGER audit_log_append_only;
UPDATE hub.audit_log SET actor_subject = :'pseudo' WHERE actor_subject = :'subject';
UPDATE hub.audit_log SET before = replace(before::text, :'subject', :'pseudo')::jsonb WHERE position(:'subject' in before::text) > 0;
UPDATE hub.audit_log SET after  = replace(after::text,  :'subject', :'pseudo')::jsonb WHERE position(:'subject' in after::text)  > 0;
ALTER TABLE hub.audit_log ENABLE TRIGGER audit_log_append_only;

UPDATE hub.posts          SET created_by = :'pseudo' WHERE created_by = :'subject';
UPDATE hub.post_revisions SET created_by = :'pseudo' WHERE created_by = :'subject';
UPDATE hub.spend_caps     SET updated_by = :'pseudo' WHERE updated_by = :'subject';
UPDATE hub.settings       SET updated_by = :'pseudo' WHERE updated_by = :'subject';
DELETE FROM hub.users WHERE subject = :'subject';   -- hub.tasks.assignee_subject becomes NULL (ON DELETE SET NULL)

-- Verification: the subject must be gone from every table and every JSON document, or nothing is committed.
-- (psql does not substitute variables inside $$ blocks, so the subject travels as a transaction-local setting.)
SELECT set_config('erase.subject', :'subject', true);
DO $check$
DECLARE s text := current_setting('erase.subject'); n bigint;
BEGIN
  SELECT (SELECT count(*) FROM hub.users WHERE subject = s)
       + (SELECT count(*) FROM hub.tasks WHERE assignee_subject = s)
       + (SELECT count(*) FROM hub.posts WHERE created_by = s)
       + (SELECT count(*) FROM hub.post_revisions WHERE created_by = s)
       + (SELECT count(*) FROM hub.spend_caps WHERE updated_by = s)
       + (SELECT count(*) FROM hub.settings WHERE updated_by = s OR position(s in value::text) > 0)
       + (SELECT count(*) FROM hub.audit_log WHERE actor_subject = s OR position(s in coalesce(before::text, '')) > 0 OR position(s in coalesce(after::text, '')) > 0)
    INTO n;
  IF n <> 0 THEN RAISE EXCEPTION 'subject still present in % place(s); rolled back', n; END IF;
  IF (SELECT tgenabled FROM pg_trigger WHERE tgname = 'audit_log_append_only' AND tgrelid = 'hub.audit_log'::regclass) <> 'O' THEN
    RAISE EXCEPTION 'the audit log trigger is not enabled again; rolled back';
  END IF;
END
$check$;

COMMIT;
\echo 'erased; the pseudonym used was:' :pseudo
