-- Full-text index over each message's text blocks (what the user and the model wrote, not tool
-- calls or their output), for the search_history tool. Trigram, so Chinese and Japanese match
-- without word splitting. Kept in step with `messages` by triggers.
CREATE VIRTUAL TABLE message_text USING fts5 (text, tokenize = 'trigram');

INSERT INTO message_text (rowid, text)
SELECT messages.id, group_concat(json_extract(block.value, '$.text'), char(10))
FROM messages, json_each(messages.content, '$.content') AS block
WHERE block.type = 'object' AND json_extract(block.value, '$.type') = 'text'
GROUP BY messages.id;

CREATE TRIGGER message_text_insert AFTER INSERT ON messages BEGIN
    INSERT INTO message_text (rowid, text)
    SELECT new.id, group_concat(json_extract(value, '$.text'), char(10))
    FROM json_each(new.content, '$.content')
    WHERE type = 'object' AND json_extract(value, '$.type') = 'text'
    HAVING count(*) > 0;
END;

CREATE TRIGGER message_text_delete AFTER DELETE ON messages BEGIN
    DELETE FROM message_text WHERE rowid = old.id;
END;
