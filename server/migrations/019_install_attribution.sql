-- Play Store install-referrer attribution: which campaign (utm_source/
-- medium/campaign) drove the install, plus the raw referrer string Play
-- handed the APK. Captured once at /api/onboard_business and immutable
-- after — this is acquisition history, not a live business setting.
ALTER TABLE businesses ADD COLUMN referral_source   TEXT;
ALTER TABLE businesses ADD COLUMN referral_medium   TEXT;
ALTER TABLE businesses ADD COLUMN referral_campaign TEXT;
ALTER TABLE businesses ADD COLUMN install_referrer  TEXT;
