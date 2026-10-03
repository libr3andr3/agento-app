-- D18: a paid plan produces a real SUNAT document, and which one depends on
-- what the buyer gave us at checkout — a RUC gets a factura, a DNI (or
-- nothing) gets a boleta. Two things were missing to do that honestly:
--
--   * a place to keep the *billing* email. An account that signed in by
--     phone alone carries a synthesized `<phone>@phone.yaya.tech` address
--     that nobody reads; a comprobante sent there is a comprobante nobody
--     receives.
--   * the document's kind, so `settle()` does not have to guess from the
--     length of a number after the fact.
ALTER TABLE checkouts ADD COLUMN customer_email TEXT;
ALTER TABLE checkouts ADD COLUMN customer_doc_type TEXT;   -- DNI | RUC

-- Which comprobante this row is. Old rows are boletas: that is all we issued.
ALTER TABLE invoices ADD COLUMN kind TEXT NOT NULL DEFAULT 'boleta';   -- boleta | factura
