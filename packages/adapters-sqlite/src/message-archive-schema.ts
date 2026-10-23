export const SCHEMA_V10_MESSAGE_ARCHIVES = `
ALTER TABLE messages ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_messages_archived ON messages(chat_id, archived, order_key);
`;
