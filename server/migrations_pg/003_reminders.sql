-- Reminder-by-calendar-invite: the customer's email + chosen lead time live
-- on the appointment; the ICS VALARM does the actual reminding client-side
-- (no outbound channel needed — fits the notification-reply design).
ALTER TABLE appointments ADD COLUMN IF NOT EXISTS customer_email TEXT;
ALTER TABLE appointments ADD COLUMN IF NOT EXISTS remind_minutes INT;
