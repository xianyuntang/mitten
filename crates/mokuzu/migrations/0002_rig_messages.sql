-- `messages.content` now holds the whole serialized rig `Message` (role, id, typed content),
-- replacing the raw Anthropic wire shape. The old rows can't be replayed, so drop them.
DELETE FROM messages;
