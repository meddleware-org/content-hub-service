# Personal data in the content hub

What the hub stores about people, why, for how long, who can see it, and how to erase it. This is a technical inventory for the
operator, not legal advice; the lawful basis, notices and the data-protection registration are the operating organisation's.

## What is personal data here

The hub is an operator tool with a handful of signed-in staff. It holds **no visitor data**: the public routes set no cookies, run
no analytics and write nothing about the people who read them. The only personal data is about **the people who sign in to the
admin console**:

| Data | Where | Source | Purpose | Retention |
| --- | --- | --- | --- | --- |
| Identity provider **subject** (the Cloudflare Access `sub`, a UUID) | `hub.users.subject`; attribution columns `hub.posts.created_by`, `hub.post_revisions.created_by`, `hub.spend_caps.updated_by`, `hub.settings.updated_by`, `hub.tasks.assignee_subject`; `hub.audit_log.actor_subject` and inside its `before`/`after` documents | the Access JWT, on each request | who changed what (accountability), task assignment | while the person works with the hub; erased on request (below) |
| **Display name** | `hub.users.display_name` | the JWT's `name` claim, else the **e-mail address**, else the subject (an earlier comment in the schema says "no e-mail": that is wrong when the identity provider sends no name, as the one-time-PIN login does, so treat this column as possibly an e-mail address) | showing "Signed in as …" and names in lists | as above |
| First and last seen | `hub.users.first_seen_at`, `last_seen_at` | server clock | showing activity | as above |

Not stored: passwords, tokens, the Access JWT, IP addresses, user agents, other e-mail addresses, anything from the identity
provider beyond the three fields above. Platform credentials are held as environment variables of the worker, not in the database
(the database stores only their NAME).

Outside the hub, by other systems: Cloudflare Access keeps its own sign-in logs (e-mail, IP, time) as the identity provider; the
cluster's request logs (nginx) hold visitor IP addresses for the machine and public hosts, with the retention the platform sets.
The hub's signed Walrus snapshots contain only public roadmap text, never people.

## Who can see it

Only signed-in admins (Access, a Keto role, CSRF) through the admin API; the public process runs as a role that cannot read any
`hub` table. Database access beyond that is the operator's (the owner, the backup role, superusers).

## Erasing a person

`scripts/erase-person.sql`, run as the schema **owner** (the application role cannot touch the audit log by design):

```bash
# on the node: a psql with the owner's URL (the `content-hub-migrate-db` Secret holds it), subject taken from hub.users
kubectl -n database exec -i postgres-0 -- psql -U postgres -d content_hub -v subject='<subject>' < scripts/erase-person.sql
```

In one transaction it replaces the subject by a random pseudonym (`erased-<uuid>`) in every attribution column and inside the
audit log's JSON, deletes the person's `hub.users` row (the display name and e-mail go with it; their task assignments become
unassigned), and verifies that the subject appears nowhere any more, or rolls everything back. The audit log keeps its entries and
its append-only trigger (re-enabled and checked inside the same transaction); only the identifier changes. It refuses a subject
the hub has never stored. `scripts/smoke.sh` step 9 proves it on every image build.

Not erased by the script, by design: backups age out by themselves (in-cluster sets: 14 daily; offsite: 14 daily, and a 30-day
bucket lifecycle rule), so a restored backup would bring the person back until the script is run again after a restore (a note in
the restore runbook). Content the person wrote (posts) stays; it is the organisation's.

## Access request (Art. 15)

`SELECT subject, display_name, first_seen_at, last_seen_at FROM hub.users WHERE subject = '<subject>';` plus the person's rows in
`hub.audit_log` (`actor_subject = '<subject>'`) and the attribution columns above give the complete set.

## Checks

- `scripts/smoke.sh` (step 9) inserts two people with posts and audit rows, erases one, and asserts that nothing of that subject
  remains, the other person's rows and the pseudonymised audit rows do, the audit trigger is enabled again, and the application role
  still cannot update the log.
