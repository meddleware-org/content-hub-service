-- content-hub schema, version 1.
--
-- Layout (docs/ARCHITECTURE.md §6):
--   * Every table lives in schema `hub`. The read-only role the public process uses has no privilege on `hub`
--     at all, so it cannot read an internal column even through a bug.
--   * The public process reads only the views `public.public_v1_*`. They expose public columns of publicly
--     visible rows and nothing else; a test pins their column lists.
--   * Enumerations are check-constrained text; the allowed values must equal the `ALL` lists of the
--     enums in `content-hub-core` (a store test compares them).
--   * Times are timestamptz (UTC); money is integer micro-US-dollars; identifiers shown publicly are
--     random `public_id` UUIDs, never the internal keys (keys can reveal internal planning).
--
-- Released migrations are never edited (SQLx records a checksum); a change is a new file that keeps the
-- previous release's queries working.

CREATE SCHEMA IF NOT EXISTS hub;

-- ── helpers ────────────────────────────────────────────────────────────────────────────────────────

CREATE FUNCTION hub.set_updated_at() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
  NEW.updated_at := now();
  RETURN NEW;
END
$$;

-- Append-only tables (audit log, spend ledger): no UPDATE, DELETE or TRUNCATE by any role that does not
-- own the table. (The owner could drop the trigger; the application roles cannot.)
CREATE FUNCTION hub.refuse_change() RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
  RAISE EXCEPTION 'table %.% is append-only', TG_TABLE_SCHEMA, TG_TABLE_NAME
    USING ERRCODE = 'insufficient_privilege';
END
$$;

-- ── people ─────────────────────────────────────────────────────────────────────────────────────────

-- Only what the admin UI shows: the identity provider's subject and a display name. No e-mail, no tokens.
CREATE TABLE hub.users (
  subject       text        PRIMARY KEY CHECK (char_length(subject) BETWEEN 1 AND 512),
  display_name  text        NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 200),
  first_seen_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at  timestamptz NOT NULL DEFAULT now()
);

-- ── roadmap ────────────────────────────────────────────────────────────────────────────────────────

CREATE TABLE hub.roadmap_phases (
  id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  key                  text        NOT NULL UNIQUE CHECK (key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  public_id            uuid        NOT NULL UNIQUE DEFAULT gen_random_uuid(),
  sort_order           integer     NOT NULL CHECK (sort_order >= 1),
  internal_name        text        NOT NULL CHECK (char_length(internal_name) BETWEEN 1 AND 200),
  internal_description text        NOT NULL DEFAULT '' CHECK (char_length(internal_description) <= 4000),
  public_name          text        NOT NULL CHECK (char_length(public_name) BETWEEN 1 AND 200),
  public_description   text        NOT NULL DEFAULT '' CHECK (char_length(public_description) <= 4000),
  window_start         timestamptz,
  window_end           timestamptz,
  visible_human        boolean     NOT NULL DEFAULT false,
  visible_bot          boolean     NOT NULL DEFAULT false,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT roadmap_phases_window CHECK (window_start IS NULL OR window_end IS NULL OR window_end >= window_start),
  -- Deferred so that two phases can swap places in one transaction.
  CONSTRAINT roadmap_phases_sort_order_key UNIQUE (sort_order) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE hub.roadmap_items (
  id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  phase_id             bigint      NOT NULL REFERENCES hub.roadmap_phases (id) ON DELETE RESTRICT,
  key                  text        NOT NULL UNIQUE CHECK (key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  public_id            uuid        NOT NULL UNIQUE DEFAULT gen_random_uuid(),
  item_type            text        NOT NULL CHECK (item_type IN (
                         'infra', 'social', 'content', 'listing', 'ad_campaign', 'research', 'engineering',
                         'bot_discoverability', 'trustless', 'other')),
  internal_title       text        NOT NULL CHECK (char_length(internal_title) BETWEEN 1 AND 200),
  internal_description text        NOT NULL DEFAULT '' CHECK (char_length(internal_description) <= 4000),
  public_title         text        CHECK (char_length(public_title) BETWEEN 1 AND 200),
  public_description   text        CHECK (char_length(public_description) <= 4000),
  visible_human        boolean     NOT NULL DEFAULT false,
  visible_bot          boolean     NOT NULL DEFAULT false,
  status               text        NOT NULL DEFAULT 'planned'
                         CHECK (status IN ('planned', 'in_progress', 'done', 'cancelled')),
  planned_at           timestamptz,
  due_at               timestamptz,
  -- The only date the public sees, and only when the operator sets it (a launch time, a countdown). Internal
  -- planning dates (planned_at, due_at) never leave the database.
  public_due_at        timestamptz,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  -- A visible item always has its own public wording: the public views never fall back to internal text.
  CONSTRAINT roadmap_items_public_wording CHECK (
    NOT (visible_human OR visible_bot) OR (public_title IS NOT NULL AND public_description IS NOT NULL))
);
CREATE INDEX roadmap_items_phase ON hub.roadmap_items (phase_id);

-- ── platforms ──────────────────────────────────────────────────────────────────────────────────────

CREATE TABLE hub.platform_definitions (
  id               bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  code             text        NOT NULL UNIQUE CHECK (code ~ '^[A-Z][A-Z0-9_]{0,40}$'),
  name             text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
  category         text        NOT NULL CHECK (category IN (
                     'social', 'forum', 'directory', 'ad_network', 'event', 'content', 'onchain', 'partnership',
                     'registry', 'owned_channel', 'internal')),
  posting_mode     text        NOT NULL CHECK (posting_mode IN ('api', 'webhook', 'feed', 'manual', 'none')),
  counting_method  text        CHECK (counting_method IN ('x_weighted', 'graphemes', 'utf16', 'codepoints', 'bytes')),
  primary_limit    integer     CHECK (primary_limit > 0),
  secondary_limits jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(secondary_limits) = 'object'),
  markup           text        NOT NULL DEFAULT 'plain'
                     CHECK (markup IN ('plain', 'markdown', 'discord_md', 'telegram_html', 'bbcode')),
  supports_media   boolean     NOT NULL DEFAULT false,
  base_url         text        CHECK (base_url ~ '^https://[^\s]+$'),
  url_templates    jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(url_templates) = 'object'),
  cost_model       jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(cost_model) = 'object'),
  enabled          boolean     NOT NULL DEFAULT false,
  notes            text        NOT NULL DEFAULT '' CHECK (char_length(notes) <= 4000),
  created_at       timestamptz NOT NULL DEFAULT now(),
  updated_at       timestamptz NOT NULL DEFAULT now(),
  -- A limit needs a way to count against it.
  CONSTRAINT platform_definitions_limit_counting CHECK (primary_limit IS NULL OR counting_method IS NOT NULL)
);

-- A destination on a platform (a channel, a subreddit, a handle, a directory listing). Credentials are never
-- stored: `secret_ref` is the NAME of an environment variable the worker reads, and the check below refuses
-- anything that looks like a URL or a token.
CREATE TABLE hub.platform_targets (
  id                     bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  platform_definition_id bigint      NOT NULL REFERENCES hub.platform_definitions (id) ON DELETE RESTRICT,
  name                   text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
  external_ref           text        CHECK (char_length(external_ref) <= 500),
  config                 jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(config) = 'object'),
  -- The variable's name must start with PLATFORM_ (content_hub_core::ident::SECRET_PREFIX), so a target cannot
  -- name any other process setting.
  secret_ref             text        CHECK (secret_ref ~ '^PLATFORM_[A-Z0-9_]{1,55}$'),
  is_sandbox             boolean     NOT NULL DEFAULT false,
  enabled                boolean     NOT NULL DEFAULT false,
  -- Real sends to this target. Off: the worker records a dry run instead. A live send also needs the worker's
  -- PUBLISHING_LIVE=1, and outside production the target must be a sandbox.
  live                   boolean     NOT NULL DEFAULT false,
  created_at             timestamptz NOT NULL DEFAULT now(),
  updated_at             timestamptz NOT NULL DEFAULT now(),
  UNIQUE (platform_definition_id, name),
  -- Lets other tables require that a target belongs to the platform they name.
  UNIQUE (id, platform_definition_id)
);

-- ── tasks ──────────────────────────────────────────────────────────────────────────────────────────

CREATE TABLE hub.tasks (
  id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  roadmap_item_id      bigint      REFERENCES hub.roadmap_items (id) ON DELETE RESTRICT,
  key                  text        NOT NULL UNIQUE CHECK (key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
  task_type            text        NOT NULL CHECK (task_type IN (
                         'ACCOUNT_SETUP', 'SOCIAL_POST', 'BLOG_ARTICLE', 'LISTING_SUBMISSION', 'AD_CAMPAIGN',
                         'KOL_CAMPAIGN', 'ENGINEERING', 'RESEARCH', 'OTHER')),
  title                text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
  description          text        NOT NULL DEFAULT '' CHECK (char_length(description) <= 4000),
  status               text        NOT NULL DEFAULT 'planned'
                         CHECK (status IN ('planned', 'ready', 'in_progress', 'done', 'blocked')),
  blocked_reason       text        CHECK (char_length(blocked_reason) BETWEEN 1 AND 1000),
  scheduled_at         timestamptz,
  completed_at         timestamptz,
  review_interval_days integer     CHECK (review_interval_days BETWEEN 1 AND 3650),
  next_review_at       timestamptz,
  assignee_subject     text        REFERENCES hub.users (subject) ON DELETE SET NULL,
  entity_gated         boolean     NOT NULL DEFAULT false,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT tasks_blocked_reason CHECK ((status = 'blocked') = (blocked_reason IS NOT NULL)),
  CONSTRAINT tasks_completed_at CHECK ((status = 'done') = (completed_at IS NOT NULL)),
  CONSTRAINT tasks_review CHECK (next_review_at IS NULL OR review_interval_days IS NOT NULL)
);
CREATE INDEX tasks_item ON hub.tasks (roadmap_item_id);
CREATE INDEX tasks_review_due ON hub.tasks (next_review_at) WHERE next_review_at IS NOT NULL;
CREATE INDEX tasks_status ON hub.tasks (status, id);

-- Cycles are refused by the application (content-hub-core); the table refuses self-dependencies.
CREATE TABLE hub.task_dependencies (
  task_id            bigint NOT NULL REFERENCES hub.tasks (id) ON DELETE CASCADE,
  depends_on_task_id bigint NOT NULL REFERENCES hub.tasks (id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, depends_on_task_id),
  CONSTRAINT task_dependencies_not_self CHECK (task_id <> depends_on_task_id)
);
CREATE INDEX task_dependencies_reverse ON hub.task_dependencies (depends_on_task_id);

CREATE TABLE hub.task_platforms (
  id                     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  task_id                bigint NOT NULL REFERENCES hub.tasks (id) ON DELETE CASCADE,
  platform_definition_id bigint NOT NULL REFERENCES hub.platform_definitions (id) ON DELETE RESTRICT,
  platform_target_id     bigint,
  -- The target (when given) must be on the named platform.
  FOREIGN KEY (platform_target_id, platform_definition_id)
    REFERENCES hub.platform_targets (id, platform_definition_id) ON DELETE RESTRICT,
  UNIQUE NULLS NOT DISTINCT (task_id, platform_definition_id, platform_target_id)
);

-- ── posts and their per-platform instances ────────────────────────────────────────────────────────

CREATE TABLE hub.posts (
  id              bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  public_id       uuid        NOT NULL UNIQUE DEFAULT gen_random_uuid(),
  title           text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 300),
  body_markdown   text        NOT NULL CHECK (char_length(body_markdown) <= 100000),
  status          text        NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'ready', 'archived')),
  tags            text[]      NOT NULL DEFAULT '{}' CHECK (cardinality(tags) <= 50),
  categories      text[]      NOT NULL DEFAULT '{}' CHECK (cardinality(categories) <= 20),
  roadmap_item_id bigint      REFERENCES hub.roadmap_items (id) ON DELETE SET NULL,
  task_id         bigint      REFERENCES hub.tasks (id) ON DELETE SET NULL,
  revision        integer     NOT NULL DEFAULT 1 CHECK (revision >= 1),
  created_by      text        NOT NULL CHECK (char_length(created_by) BETWEEN 1 AND 512),
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE hub.post_revisions (
  id            bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  post_id       bigint      NOT NULL REFERENCES hub.posts (id) ON DELETE CASCADE,
  revision      integer     NOT NULL CHECK (revision >= 1),
  title         text        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 300),
  body_markdown text        NOT NULL CHECK (char_length(body_markdown) <= 100000),
  created_by    text        NOT NULL CHECK (char_length(created_by) BETWEEN 1 AND 512),
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (post_id, revision)
);

-- One canonical post rendered for one target. The row is also the unit of work of the publishing queue
-- (claimed with FOR UPDATE SKIP LOCKED; see ARCHITECTURE.md §10), so there is no separate outbox to keep in
-- step. History is kept: a post with instances cannot be deleted (RESTRICT).
CREATE TABLE hub.platform_instances (
  id                       bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  post_id                  bigint      NOT NULL REFERENCES hub.posts (id) ON DELETE RESTRICT,
  platform_target_id       bigint      NOT NULL REFERENCES hub.platform_targets (id) ON DELETE RESTRICT,
  override_title           text        CHECK (char_length(override_title) BETWEEN 1 AND 300),
  override_body            text        CHECK (char_length(override_body) <= 100000),
  status                   text        NOT NULL DEFAULT 'planned' CHECK (status IN (
                             'planned', 'queued', 'sending', 'posted', 'failed', 'unknown', 'manual_required',
                             'cancelled')),
  external_status          text        CHECK (external_status IN (
                             'proposed', 'confirmed', 'published', 'submitted', 'in_review', 'approved', 'rejected')),
  url_or_external_id       text        CHECK (char_length(url_or_external_id) BETWEEN 1 AND 2000),
  scheduled_at             timestamptz,
  posted_at                timestamptz,
  -- Set when the worker commits `sending` (before the external call); the janitor moves a stale row to
  -- `unknown`, never back to `queued`.
  sending_started_at       timestamptz,
  attempt_count            integer     NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_attempt_at          timestamptz,
  last_error               text        CHECK (char_length(last_error) <= 2000),
  idempotency_key          uuid        NOT NULL UNIQUE DEFAULT gen_random_uuid(),
  cost_estimate_usd_micros bigint      NOT NULL DEFAULT 0 CHECK (cost_estimate_usd_micros >= 0),
  metrics                  jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(metrics) = 'object'),
  -- The payload exactly as validated when it was last queued (rendered for the platform; the worker sends these
  -- bytes and nothing else), the post revision it came from, and its SHA-256.
  post_revision            integer     CHECK (post_revision >= 1),
  payload_title            text        CHECK (char_length(payload_title) BETWEEN 1 AND 300),
  payload_body             text        CHECK (char_length(payload_body) <= 200000),
  payload_sha256           text        CHECK (payload_sha256 ~ '^[0-9a-f]{64}$'),
  created_at               timestamptz NOT NULL DEFAULT now(),
  updated_at               timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT platform_instances_posted_at CHECK (status <> 'posted' OR posted_at IS NOT NULL),
  CONSTRAINT platform_instances_sending CHECK ((status = 'sending') = (sending_started_at IS NOT NULL)),
  CONSTRAINT platform_instances_payload CHECK (
    (payload_sha256 IS NULL) = (payload_body IS NULL) AND (payload_sha256 IS NULL) = (payload_title IS NULL)
    AND (payload_sha256 IS NULL) = (post_revision IS NULL)),
  -- Nothing is queued or sent without a validated payload.
  CONSTRAINT platform_instances_payload_before_send CHECK (status NOT IN ('queued', 'sending') OR payload_sha256 IS NOT NULL)
);
-- At most one live instance of a post per target: a second send to the same place is refused by the
-- database, not only by the UI. A cancelled instance does not count.
CREATE UNIQUE INDEX platform_instances_one_live
  ON hub.platform_instances (post_id, platform_target_id) WHERE status <> 'cancelled';
-- The worker's claim query and the janitor's scan.
CREATE INDEX platform_instances_due
  ON hub.platform_instances ((COALESCE(next_attempt_at, scheduled_at)), id) WHERE status = 'queued';
CREATE INDEX platform_instances_in_flight ON hub.platform_instances (sending_started_at) WHERE status = 'sending';
CREATE INDEX platform_instances_target ON hub.platform_instances (platform_target_id, status);

CREATE TABLE hub.delivery_attempts (
  id                      bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  platform_instance_id    bigint      NOT NULL REFERENCES hub.platform_instances (id) ON DELETE RESTRICT,
  started_at              timestamptz NOT NULL DEFAULT now(),
  finished_at             timestamptz,
  -- NULL while the attempt is in flight.
  outcome                 text        CHECK (outcome IN (
                            'success', 'failed', 'unknown', 'rate_limited', 'validation_error', 'dry_run')),
  http_status             integer     CHECK (http_status BETWEEN 100 AND 599),
  -- Redacted before it is stored (no tokens, webhook URLs or cookies).
  response_excerpt        text        CHECK (char_length(response_excerpt) <= 2000),
  request_redacted        jsonb,
  dry_run                 boolean     NOT NULL,
  rendered_payload_sha256 text        NOT NULL CHECK (rendered_payload_sha256 ~ '^[0-9a-f]{64}$'),
  CONSTRAINT delivery_attempts_finished CHECK (finished_at IS NULL OR finished_at >= started_at),
  CONSTRAINT delivery_attempts_outcome_finished CHECK ((outcome IS NULL) = (finished_at IS NULL)),
  CONSTRAINT delivery_attempts_dry_run CHECK (outcome IS NULL OR (outcome = 'dry_run') = dry_run)
);
CREATE INDEX delivery_attempts_instance ON hub.delivery_attempts (platform_instance_id, id);

-- ── task links (after posts and instances, which they reference) ─────────────────────────────────

CREATE TABLE hub.task_links (
  id                   bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  task_id              bigint      NOT NULL REFERENCES hub.tasks (id) ON DELETE CASCADE,
  link_kind            text        NOT NULL CHECK (link_kind IN ('post', 'platform_instance', 'external')),
  post_id              bigint      REFERENCES hub.posts (id) ON DELETE CASCADE,
  platform_instance_id bigint      REFERENCES hub.platform_instances (id) ON DELETE CASCADE,
  url                  text        CHECK (url ~ '^https://[^\s]+$' AND char_length(url) <= 2000),
  label                text        NOT NULL DEFAULT '' CHECK (char_length(label) <= 200),
  created_at           timestamptz NOT NULL DEFAULT now(),
  -- Exactly the column that matches the kind is set.
  CONSTRAINT task_links_kind CHECK (
    (link_kind = 'post' AND post_id IS NOT NULL AND platform_instance_id IS NULL AND url IS NULL)
    OR (link_kind = 'platform_instance' AND platform_instance_id IS NOT NULL AND post_id IS NULL AND url IS NULL)
    OR (link_kind = 'external' AND url IS NOT NULL AND post_id IS NULL AND platform_instance_id IS NULL))
);
CREATE INDEX task_links_task ON hub.task_links (task_id);

-- ── budgets ────────────────────────────────────────────────────────────────────────────────────────

-- Runtime caps (layer 3 and 4 of ARCHITECTURE.md §2 "Cost caps"). The instance ceiling (layer 2) is
-- configuration, not data; the application refuses a row above it.
CREATE TABLE hub.spend_caps (
  id                       bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  scope                    text        NOT NULL CHECK (scope IN ('total', 'platform', 'target')),
  platform_definition_id   bigint      REFERENCES hub.platform_definitions (id) ON DELETE CASCADE,
  platform_target_id       bigint      REFERENCES hub.platform_targets (id) ON DELETE CASCADE,
  monthly_limit_usd_micros bigint      NOT NULL CHECK (monthly_limit_usd_micros >= 0),
  daily_post_limit         integer     CHECK (daily_post_limit > 0),
  updated_by               text        NOT NULL CHECK (char_length(updated_by) BETWEEN 1 AND 512),
  updated_at               timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT spend_caps_scope CHECK (
    (scope = 'total' AND platform_definition_id IS NULL AND platform_target_id IS NULL)
    OR (scope = 'platform' AND platform_definition_id IS NOT NULL AND platform_target_id IS NULL)
    OR (scope = 'target' AND platform_target_id IS NOT NULL AND platform_definition_id IS NULL)),
  UNIQUE NULLS NOT DISTINCT (scope, platform_definition_id, platform_target_id)
);

-- Spend is metered BEFORE the action: `reserve` in the enqueue transaction (refused at the cap), then
-- `settle` or `release`. Append-only.
CREATE TABLE hub.spend_ledger (
  id                     bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at                     timestamptz NOT NULL DEFAULT now(),
  period_month           date        NOT NULL CHECK (period_month = date_trunc('month', period_month)::date),
  platform_definition_id bigint      NOT NULL REFERENCES hub.platform_definitions (id) ON DELETE RESTRICT,
  platform_target_id     bigint      REFERENCES hub.platform_targets (id) ON DELETE RESTRICT,
  platform_instance_id   bigint      REFERENCES hub.platform_instances (id) ON DELETE RESTRICT,
  kind                   text        NOT NULL CHECK (kind IN ('reserve', 'settle', 'release')),
  amount_usd_micros      bigint      NOT NULL CHECK (amount_usd_micros >= 0),
  note                   text        NOT NULL DEFAULT '' CHECK (char_length(note) <= 500)
);
CREATE INDEX spend_ledger_period ON hub.spend_ledger (period_month, platform_definition_id);

-- Runtime settings (for example `publishing.paused`, the worker's stop switch). Values are JSON; the application validates each
-- key's shape.
CREATE TABLE hub.settings (
  key        text        PRIMARY KEY CHECK (key ~ '^[a-z][a-z0-9_.]{0,100}$'),
  value      jsonb       NOT NULL,
  updated_by text        NOT NULL CHECK (char_length(updated_by) BETWEEN 1 AND 512),
  updated_at timestamptz NOT NULL DEFAULT now()
);

-- ── audit ──────────────────────────────────────────────────────────────────────────────────────────

CREATE TABLE hub.audit_log (
  id            bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at            timestamptz NOT NULL DEFAULT now(),
  actor_subject text        NOT NULL CHECK (char_length(actor_subject) BETWEEN 1 AND 512),
  action        text        NOT NULL CHECK (action ~ '^[a-z][a-z0-9_.]{0,63}$'),
  entity_type   text        NOT NULL CHECK (entity_type ~ '^[a-z][a-z0-9_]{0,63}$'),
  entity_id     text        CHECK (char_length(entity_id) <= 200),
  before        jsonb,
  after         jsonb
);
CREATE INDEX audit_log_entity ON hub.audit_log (entity_type, entity_id, id);

-- ── trustless snapshots (ADR-0006/0007; written by export-snapshot) ──────────────────────────────

CREATE TABLE hub.snapshots (
  id             bigint      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  network        text        NOT NULL CHECK (network IN ('localnet', 'testnet', 'mainnet')),
  generated_at   timestamptz NOT NULL,
  schema_version integer     NOT NULL CHECK (schema_version >= 1),
  sha256         text        NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
  byte_size      integer     NOT NULL CHECK (byte_size BETWEEN 1 AND 1048576),
  signature      text        NOT NULL CHECK (char_length(signature) BETWEEN 1 AND 1000),
  signer_address text        NOT NULL CHECK (signer_address ~ '^0x[0-9a-f]{64}$'),
  blob_id        text        CHECK (char_length(blob_id) BETWEEN 1 AND 200),
  object_id      text        CHECK (object_id ~ '^0x[0-9a-f]{64}$'),
  epochs         integer     CHECK (epochs BETWEEN 1 AND 53),
  expires_at     timestamptz,
  created_at     timestamptz NOT NULL DEFAULT now(),
  UNIQUE (network, sha256),
  -- A stored snapshot knows when it expires; an unstored one has neither.
  CONSTRAINT snapshots_stored CHECK ((blob_id IS NULL) = (expires_at IS NULL) AND (blob_id IS NULL) = (epochs IS NULL))
);

-- ── triggers ───────────────────────────────────────────────────────────────────────────────────────

CREATE TRIGGER roadmap_phases_updated_at BEFORE UPDATE ON hub.roadmap_phases
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER roadmap_items_updated_at BEFORE UPDATE ON hub.roadmap_items
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER platform_definitions_updated_at BEFORE UPDATE ON hub.platform_definitions
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER platform_targets_updated_at BEFORE UPDATE ON hub.platform_targets
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER tasks_updated_at BEFORE UPDATE ON hub.tasks
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER posts_updated_at BEFORE UPDATE ON hub.posts
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();
CREATE TRIGGER platform_instances_updated_at BEFORE UPDATE ON hub.platform_instances
  FOR EACH ROW EXECUTE FUNCTION hub.set_updated_at();

CREATE TRIGGER audit_log_append_only BEFORE UPDATE OR DELETE ON hub.audit_log
  FOR EACH ROW EXECUTE FUNCTION hub.refuse_change();
CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON hub.audit_log
  FOR EACH STATEMENT EXECUTE FUNCTION hub.refuse_change();
CREATE TRIGGER spend_ledger_append_only BEFORE UPDATE OR DELETE ON hub.spend_ledger
  FOR EACH ROW EXECUTE FUNCTION hub.refuse_change();
CREATE TRIGGER spend_ledger_no_truncate BEFORE TRUNCATE ON hub.spend_ledger
  FOR EACH STATEMENT EXECUTE FUNCTION hub.refuse_change();

-- ── public views (the public process's only data source) ─────────────────────────────────────────
--
-- Owned by the migrating role and run with its privileges (security_invoker off), so the read-only role
-- needs SELECT on these views only. `security_barrier` keeps a caller's own predicates from being
-- evaluated before the visibility filter. Columns are fixed by a test (store::tests::public_view_columns).

-- A phase is public when it is visible to either audience.
CREATE VIEW public.public_v1_phases WITH (security_barrier = true) AS
SELECT p.public_id,
       p.sort_order,
       p.public_name        AS name,
       p.public_description AS description,
       p.window_start,
       p.window_end,
       p.visible_human,
       p.visible_bot,
       p.updated_at
  FROM hub.roadmap_phases p
 WHERE p.visible_human OR p.visible_bot;

-- An item is visible to an audience only when its phase is visible to that audience too, so an item can
-- never surface a hidden phase. No key, type or internal date leaves the database; `due_at` is the item's
-- public_due_at, NULL unless the operator chose to publish one.
CREATE VIEW public.public_v1_items WITH (security_barrier = true) AS
SELECT i.public_id,
       p.public_id                         AS phase_public_id,
       i.public_title                      AS title,
       i.public_description                AS description,
       i.status,
       i.public_due_at                     AS due_at,
       (i.visible_human AND p.visible_human) AS visible_human,
       (i.visible_bot AND p.visible_bot)     AS visible_bot,
       i.updated_at
  FROM hub.roadmap_items i
  JOIN hub.roadmap_phases p ON p.id = i.phase_id
 WHERE (i.visible_human AND p.visible_human) OR (i.visible_bot AND p.visible_bot);

-- Posts published to an owned feed (posting_mode 'feed'): the instance is posted, its target enabled, and the
-- post is not a draft. The instance's overrides win over the canonical text, as on every other platform.
CREATE VIEW public.public_v1_feed_entries WITH (security_barrier = true) AS
SELECT po.public_id,
       COALESCE(pi.override_title, po.title)        AS title,
       COALESCE(pi.override_body, po.body_markdown) AS body_markdown,
       pi.posted_at                                 AS published_at,
       GREATEST(po.updated_at, pi.updated_at)       AS updated_at
  FROM hub.platform_instances pi
  JOIN hub.posts po ON po.id = pi.post_id
  JOIN hub.platform_targets t ON t.id = pi.platform_target_id
  JOIN hub.platform_definitions d ON d.id = t.platform_definition_id
 WHERE pi.status = 'posted'
   AND d.posting_mode = 'feed'
   AND t.enabled
   AND po.status <> 'draft';

-- Signed public snapshots: every field is public by design (they are published on Walrus).
CREATE VIEW public.public_v1_snapshots WITH (security_barrier = true) AS
SELECT s.network,
       s.generated_at,
       s.schema_version,
       s.sha256,
       s.byte_size,
       s.signature,
       s.signer_address,
       s.blob_id,
       s.object_id,
       s.epochs,
       s.expires_at
  FROM hub.snapshots s;
