-- OpenID Connect sign-in for the admin console (docs/auth/CONSOLE_SIGNIN.md in the platform workspace): the sign-ins in progress and the signed-in
-- sessions. Only hashes of the browser's secrets are stored, so a copy of these tables holds no usable cookie.

-- A sign-in the browser has started and not finished. One row is consumed (deleted) by the callback, so a state value works once.
CREATE TABLE hub.login_attempts (
  -- SHA-256 (base64url) of the `state` sent to the provider.
  state_hash   text        PRIMARY KEY CHECK (char_length(state_hash) BETWEEN 16 AND 64),
  -- SHA-256 of the value in the short-lived cookie that ties the callback to the browser that started the sign-in (login CSRF defence).
  binding_hash text        NOT NULL CHECK (char_length(binding_hash) BETWEEN 16 AND 64),
  -- The `nonce` the ID token must carry.
  nonce        text        NOT NULL CHECK (char_length(nonce) BETWEEN 16 AND 128),
  -- The PKCE verifier, sent only in the token request.
  verifier     text        NOT NULL CHECK (char_length(verifier) BETWEEN 43 AND 128),
  created_at   timestamptz NOT NULL DEFAULT now()
);

-- A signed-in operator's session. The cookie holds the random identifier; the table holds its SHA-256.
CREATE TABLE hub.admin_sessions (
  id_hash      text        PRIMARY KEY CHECK (char_length(id_hash) BETWEEN 16 AND 64),
  -- The provider's `sub` for the person (the identifier OpenFGA knows).
  subject      text        NOT NULL CHECK (char_length(subject) BETWEEN 1 AND 512),
  -- The provider's session id, which a back-channel logout names (empty when the provider sent none).
  provider_sid text        NOT NULL DEFAULT '' CHECK (char_length(provider_sid) <= 512),
  -- The name shown in the console.
  display_name text        NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 200),
  -- The ID token, kept only to end the provider's session at sign-out (`id_token_hint`).
  id_token     text        NOT NULL CHECK (char_length(id_token) <= 16384),
  created_at   timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz NOT NULL DEFAULT now()
);
-- Back-channel logout finds sessions by the person or by the provider's session.
CREATE INDEX admin_sessions_subject ON hub.admin_sessions (subject);
CREATE INDEX admin_sessions_sid ON hub.admin_sessions (provider_sid) WHERE provider_sid <> '';
