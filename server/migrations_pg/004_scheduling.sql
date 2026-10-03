-- Service-aware scheduling: real businesses have per-service durations
-- (corte 30min, pintado completo 120min) and staff capacity — bookings must
-- block the right amount of calendar, not one fixed slot.
ALTER TABLE appointments ADD COLUMN IF NOT EXISTS service TEXT;
ALTER TABLE appointments ADD COLUMN IF NOT EXISTS duration_mins INT;
