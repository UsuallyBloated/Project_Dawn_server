-- Session tokens are stored as their SHA-256 from this build on
-- (db::session_key), so a copy of the database carries no redeemable session.
-- Every row written before this holds a raw token that no hashed lookup will
-- ever match, so they go. The cost is one fresh login per launcher, which the
-- cutover day asks for anyway. Forward-only, like every migration here.
DELETE FROM sessions;
