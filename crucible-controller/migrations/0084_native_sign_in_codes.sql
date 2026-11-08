-- 0084_native_sign_in_codes.sql — single-use codes a desktop app redeems, with the PKCE verifier
-- it kept, for an api key its person approved in the browser. A code lives for a minute and is
-- deleted when redeemed; the secret it buys is never stored here.
CREATE TABLE native_sign_in_codes (
    -- SHA-256 of the code, base64url. The code itself only ever travels in the redirect.
    code_hash    TEXT PRIMARY KEY,
    sub          TEXT NOT NULL REFERENCES users(sub) ON DELETE CASCADE,
    -- The key's name, as the approval page showed it.
    name         TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    -- The S256 challenge the redeeming verifier must hash to.
    challenge    TEXT NOT NULL,
    expires_at   TEXT NOT NULL
);
