-- A Yape sale is a sale: it needs the same SUNAT document a card sale gets.
-- `checkouts` learned these in 026; `plan_requests` — the row behind every
-- Yape/Plin purchase, which is how almost every Peruvian owner actually pays
-- — never had them, so `confirm_request` had nothing to put on a comprobante
-- and issued none at all.
--
-- Null is the honest default for rows that predate this: a boleta may go out
-- "sin documento" under S/700, and above that we would rather issue by hand
-- than guess at who bought.
ALTER TABLE plan_requests ADD COLUMN customer_doc TEXT;
ALTER TABLE plan_requests ADD COLUMN customer_doc_type TEXT;   -- DNI | RUC
ALTER TABLE plan_requests ADD COLUMN customer_email TEXT;
